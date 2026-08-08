use crate::error::AppError;
use crate::models::profile::{ArchiveMeta, ProfileArchive};
use crate::services::fsops::{self, Transfer};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct SnapshotItem {
    pub name: &'static str,
    pub path: PathBuf,
    /// Relative paths kept in the live tool directory and excluded from archives.
    pub preserve: &'static [&'static str],
}

pub struct SnapshotSpec {
    pub source: &'static str,
    pub display_name: &'static str,
    pub archive_root: PathBuf,
    pub items: Vec<SnapshotItem>,
    pub clear_current_on_create: bool,
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn preserve_paths(item: &SnapshotItem) -> Vec<PathBuf> {
    item.preserve.iter().map(PathBuf::from).collect()
}

fn clear_current_items(spec: &SnapshotSpec) -> Result<(), AppError> {
    for item in &spec.items {
        fsops::remove_tree_except(&item.path, &preserve_paths(item))?;
    }
    Ok(())
}

fn archive_path(spec: &SnapshotSpec, name: &str) -> PathBuf {
    spec.archive_root.join(name)
}

fn existing_items(spec: &SnapshotSpec) -> Vec<SnapshotItem> {
    spec.items
        .iter()
        .filter(|item| fsops::has_data(&item.path, &preserve_paths(item)))
        .cloned()
        .collect()
}

pub fn list_profiles(spec: &SnapshotSpec) -> Result<Vec<ProfileArchive>, AppError> {
    let mut profiles = Vec::new();
    if !spec.archive_root.exists() {
        return Ok(profiles);
    }

    for entry in fs::read_dir(&spec.archive_root)?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let meta_path = entry.path().join("_meta.json");
        if let Ok(content) = fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<ArchiveMeta>(&content) {
                profiles.push(ProfileArchive {
                    source: spec.source.to_string(),
                    source_display_name: spec.display_name.to_string(),
                    is_auto: name.starts_with("auto-"),
                    name,
                    created: meta.created,
                    items: meta.items,
                    total_size: meta.total_size,
                    size_human: meta.size_human,
                    note: meta.note,
                });
                continue;
            }
        }
        profiles.push(ProfileArchive {
            source: spec.source.to_string(),
            source_display_name: spec.display_name.to_string(),
            is_auto: name.starts_with("auto-"),
            created: String::new(),
            items: 0,
            total_size: 0,
            size_human: "?".to_string(),
            note: None,
            name,
        });
    }

    profiles.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(profiles)
}

/// Put already-archived items back in the live directory. Only meaningful for a taking snapshot:
/// the data was moved, so an aborted archive would otherwise strand it in a directory we are about
/// to delete.
fn rollback_taken(archive: &Path, taken: &[SnapshotItem]) {
    for item in taken {
        let from = archive.join(item.name);
        if !from.exists() {
            continue;
        }
        if let Some(parent) = item.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if !item.path.exists() && fs::rename(&from, &item.path).is_ok() {
            continue;
        }
        let _ = fsops::copy_tree(&from, &item.path, &[]);
    }
}

/// Transfer one item into the archive. `take` moves (the live copy is meant to go away anyway, and
/// a same-volume rename makes that instant); otherwise it copies and checks the result against a
/// stat of the source, which is the same completeness guarantee the old double-walk gave.
fn archive_item(item: &SnapshotItem, dst: &Path, take: bool) -> Result<Transfer, AppError> {
    let preserve = preserve_paths(item);
    if take {
        return fsops::move_tree(&item.path, dst, &preserve);
    }
    let expected = fsops::dir_stats(&item.path, &preserve);
    let copied = fsops::copy_tree(&item.path, dst, &preserve)?;
    if copied != expected {
        return Err(AppError::Archive(format!(
            "copy verification failed for {} (source {} files/{} bytes, archive {} files/{} bytes)",
            item.name, expected.files, expected.bytes, copied.files, copied.bytes
        )));
    }
    Ok(copied)
}

fn write_archive(
    spec: &SnapshotSpec,
    archive_name: String,
    note: Option<String>,
    take: bool,
) -> Result<ProfileArchive, AppError> {
    let items = existing_items(spec);
    if items.is_empty() {
        return Err(AppError::Archive(format!(
            "{} has no snapshot data",
            spec.display_name
        )));
    }

    let path = archive_path(spec, &archive_name);
    if path.exists() {
        return Err(AppError::Archive(format!(
            "snapshot already exists: {}",
            archive_name
        )));
    }
    fs::create_dir_all(&path)?;

    let mut total_size = 0u64;
    let mut archived: Vec<SnapshotItem> = Vec::new();
    for item in &items {
        match archive_item(item, &path.join(item.name), take) {
            Ok(transfer) => {
                total_size += transfer.bytes;
                archived.push(item.clone());
            }
            Err(err) => {
                if take {
                    rollback_taken(&path, &archived);
                }
                let _ = fsops::remove_tree(&path);
                return Err(AppError::Archive(format!(
                    "archiving {} failed: {}",
                    item.name, err
                )));
            }
        }
    }

    let meta = ArchiveMeta {
        created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        name: archive_name.clone(),
        items: archived.len() as u32,
        total_size,
        size_human: format_size(total_size),
        note,
    };
    if let Err(err) = fs::write(
        path.join("_meta.json"),
        serde_json::to_string_pretty(&meta)?,
    ) {
        if take {
            rollback_taken(&path, &archived);
        }
        let _ = fsops::remove_tree(&path);
        return Err(AppError::Archive(format!(
            "writing snapshot metadata failed: {}",
            err
        )));
    }

    Ok(ProfileArchive {
        source: spec.source.to_string(),
        source_display_name: spec.display_name.to_string(),
        name: archive_name,
        created: meta.created,
        items: meta.items,
        total_size,
        size_human: meta.size_human,
        note: meta.note,
        is_auto: false,
    })
}

pub fn create_profile(
    spec: &SnapshotSpec,
    label: Option<String>,
) -> Result<ProfileArchive, AppError> {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let archive_name = match label.filter(|label| !label.trim().is_empty()) {
        Some(label) => format!("{}-{}", timestamp, label.trim()),
        None => timestamp,
    };
    let archive = write_archive(spec, archive_name, None, spec.clear_current_on_create)?;
    if spec.clear_current_on_create {
        // The archived items already moved out; this only sweeps up what was left behind
        // (empty directories, and items that held nothing worth archiving).
        clear_current_items(spec)?;
    }
    Ok(archive)
}

pub fn restore_profile(spec: &SnapshotSpec, name: &str) -> Result<(), AppError> {
    let path = archive_path(spec, name);
    if !path.exists() {
        return Err(AppError::NotFound(format!("snapshot not found: {}", name)));
    }

    if !existing_items(spec).is_empty() {
        // The current state is cleared immediately below, so the backup may take it rather than
        // duplicate it.
        write_archive(
            spec,
            format!("auto-{}", Local::now().format("%Y%m%d-%H%M%S")),
            Some(format!("Automatic backup before restoring {}", name)),
            true,
        )?;
    }

    clear_current_items(spec)?;

    for item in &spec.items {
        let src = path.join(item.name);
        if !src.exists() {
            continue;
        }
        // Copy, never move: the archive has to survive being restored from.
        fsops::copy_tree(&src, &item.path, &preserve_paths(item))?;
    }

    Ok(())
}

pub fn delete_profile(spec: &SnapshotSpec, name: &str) -> Result<(), AppError> {
    let path = archive_path(spec, name);
    if !path.exists() {
        return Err(AppError::NotFound(format!("snapshot not found: {}", name)));
    }
    fsops::remove_tree(&path)
}

pub fn rename_profile(spec: &SnapshotSpec, old: &str, new: &str) -> Result<(), AppError> {
    let old_path = archive_path(spec, old);
    let new_path = archive_path(spec, new);
    if !old_path.exists() {
        return Err(AppError::NotFound(format!("snapshot not found: {}", old)));
    }
    if new_path.exists() {
        return Err(AppError::Archive(format!(
            "snapshot already exists: {}",
            new
        )));
    }
    fs::rename(&old_path, &new_path)?;
    let meta_path = new_path.join("_meta.json");
    if meta_path.exists() {
        let content = fs::read_to_string(&meta_path)?;
        if let Ok(mut meta) = serde_json::from_str::<ArchiveMeta>(&content) {
            meta.name = new.to_string();
            fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "code-dejavu-{}-{}-{}",
                name,
                std::process::id(),
                nonce
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn create_clears_current_data_but_preserves_auth_and_restore_keeps_current_auth() {
        let temp = TestDir::new("profile-archiver");
        let current = temp.path.join("current");
        let archives = temp.path.join("archives");
        fs::create_dir_all(current.join("sessions")).expect("sessions");
        fs::create_dir_all(current.join("nested")).expect("nested");
        fs::write(current.join("auth.json"), "current-auth").expect("auth");
        fs::write(current.join("nested").join("token.json"), "current-token").expect("token");
        fs::write(current.join("nested").join("state.json"), "state").expect("nested state");
        fs::write(current.join("config.toml"), "config").expect("config");
        fs::write(current.join("sessions").join("rollout.jsonl"), "session").expect("session");

        let spec = SnapshotSpec {
            source: "test",
            display_name: "Test Agent",
            archive_root: archives.clone(),
            items: vec![SnapshotItem {
                name: "agent",
                path: current.clone(),
                preserve: &["auth.json", "nested/token.json"],
            }],
            clear_current_on_create: true,
        };

        let archive = create_profile(&spec, Some("case".to_string())).expect("create profile");
        let archived_agent = archives.join(&archive.name).join("agent");
        assert!(archived_agent.join("config.toml").exists());
        assert!(archived_agent
            .join("sessions")
            .join("rollout.jsonl")
            .exists());
        assert!(archived_agent.join("nested").join("state.json").exists());
        assert!(!archived_agent.join("auth.json").exists());
        assert!(!archived_agent.join("nested").join("token.json").exists());

        assert!(current.join("auth.json").exists());
        assert!(current.join("nested").join("token.json").exists());
        assert!(!current.join("config.toml").exists());
        assert!(!current.join("sessions").exists());
        assert!(!current.join("nested").join("state.json").exists());

        fs::write(current.join("auth.json"), "fresh-auth").expect("fresh auth");
        restore_profile(&spec, &archive.name).expect("restore profile");

        assert_eq!(
            fs::read_to_string(current.join("auth.json")).expect("read auth"),
            "fresh-auth"
        );
        assert_eq!(
            fs::read_to_string(current.join("nested").join("token.json")).expect("read token"),
            "current-token"
        );
        assert!(current.join("config.toml").exists());
        assert!(current.join("sessions").join("rollout.jsonl").exists());
        assert!(current.join("nested").join("state.json").exists());
    }

    #[test]
    fn taking_snapshot_leaves_no_temporary_stash_next_to_the_live_directory() {
        let temp = TestDir::new("profile-archiver-stash");
        let current = temp.path.join("current");
        fs::create_dir_all(&current).expect("current");
        fs::write(current.join("auth.json"), "secret").expect("auth");
        fs::write(current.join("config.toml"), "config").expect("config");

        let spec = SnapshotSpec {
            source: "test",
            display_name: "Test Agent",
            archive_root: temp.path.join("archives"),
            items: vec![SnapshotItem {
                name: "agent",
                path: current.clone(),
                preserve: &["auth.json"],
            }],
            clear_current_on_create: true,
        };

        create_profile(&spec, None).expect("create profile");

        let leftovers: Vec<String> = fs::read_dir(&temp.path)
            .expect("read temp")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(".dejavu-keep-"))
            .collect();
        assert!(leftovers.is_empty(), "stash left behind: {:?}", leftovers);
        assert_eq!(
            fs::read_to_string(current.join("auth.json")).expect("auth kept"),
            "secret"
        );
    }

    #[test]
    fn snapshot_that_does_not_clear_current_data_keeps_it_in_place() {
        let temp = TestDir::new("profile-archiver-copy");
        let current = temp.path.join("current");
        fs::create_dir_all(&current).expect("current");
        fs::write(current.join("config.toml"), "config").expect("config");

        let spec = SnapshotSpec {
            source: "test",
            display_name: "Test Agent",
            archive_root: temp.path.join("archives"),
            items: vec![SnapshotItem {
                name: "agent",
                path: current.clone(),
                preserve: &[],
            }],
            clear_current_on_create: false,
        };

        let archive = create_profile(&spec, None).expect("create profile");

        assert_eq!(archive.total_size, 6);
        assert!(current.join("config.toml").exists());
        assert!(temp
            .path
            .join("archives")
            .join(&archive.name)
            .join("agent")
            .join("config.toml")
            .exists());
    }
}
