use crate::error::AppError;
use crate::models::profile::{ArchiveMeta, ProfileArchive};
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

fn should_preserve(rel: &Path, preserve: &[PathBuf]) -> bool {
    preserve
        .iter()
        .any(|preserved| rel == preserved || rel.starts_with(preserved))
}

fn has_preserved_descendant(rel: &Path, preserve: &[PathBuf]) -> bool {
    preserve
        .iter()
        .any(|preserved| preserved.starts_with(rel) && preserved != rel)
}

fn dir_size_recursive(path: &Path, preserve: &[PathBuf], rel: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    if should_preserve(rel, preserve) {
        return 0;
    }
    if path.is_file() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| {
                    let child_rel = rel.join(entry.file_name());
                    dir_size_recursive(&entry.path(), preserve, &child_rel)
                })
                .sum()
        })
        .unwrap_or(0)
}

fn file_count_recursive(path: &Path, preserve: &[PathBuf], rel: &Path) -> usize {
    if !path.exists() || should_preserve(rel, preserve) {
        return 0;
    }
    if path.is_file() {
        return 1;
    }
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| {
                    let child_rel = rel.join(entry.file_name());
                    file_count_recursive(&entry.path(), preserve, &child_rel)
                })
                .sum()
        })
        .unwrap_or(0)
}

fn has_archivable_data(path: &Path, preserve: &[PathBuf], rel: &Path) -> bool {
    if !path.exists() || should_preserve(rel, preserve) {
        return false;
    }
    if path.is_file() {
        return true;
    }
    fs::read_dir(path)
        .map(|entries| {
            entries.filter_map(|entry| entry.ok()).any(|entry| {
                let child_rel = rel.join(entry.file_name());
                has_archivable_data(&entry.path(), preserve, &child_rel)
            })
        })
        .unwrap_or(false)
}

fn remove_path(path: &Path) -> Result<(), AppError> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.is_file() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    preserve: &[PathBuf],
    rel: &Path,
) -> Result<(), AppError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let src_path = entry.path();
        let child_rel = rel.join(entry.file_name());
        if should_preserve(&child_rel, preserve) {
            continue;
        }
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path, preserve, &child_rel)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn copy_item_with_preserve(src: &Path, dst: &Path, preserve: &[PathBuf]) -> Result<(), AppError> {
    if src.is_dir() {
        copy_dir_recursive(src, dst, preserve, Path::new(""))
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }
}

fn verify_copy(src: &Path, dst: &Path, preserve: &[PathBuf]) -> bool {
    if !dst.exists() {
        return false;
    }
    if src.is_file() && dst.is_file() {
        let src_len = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let dst_len = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        return src_len == dst_len;
    }
    if src.is_dir() && dst.is_dir() {
        let src_count = file_count_recursive(src, preserve, Path::new(""));
        let dst_count = file_count_recursive(dst, &[], Path::new(""));
        let src_size = dir_size_recursive(src, preserve, Path::new(""));
        let dst_size = dir_size_recursive(dst, &[], Path::new(""));
        return src_count == dst_count && src_size == dst_size;
    }
    false
}

fn remove_children_except(root: &Path, preserve: &[PathBuf], rel: &Path) -> Result<(), AppError> {
    if !root.exists() {
        return Ok(());
    }
    if root.is_file() {
        if !should_preserve(rel, preserve) {
            remove_path(root)?;
        }
        return Ok(());
    }

    for entry in fs::read_dir(root)?.flatten() {
        let child_path = entry.path();
        let child_rel = rel.join(entry.file_name());
        if should_preserve(&child_rel, preserve) {
            continue;
        }
        if child_path.is_dir() && has_preserved_descendant(&child_rel, preserve) {
            remove_children_except(&child_path, preserve, &child_rel)?;
            if fs::read_dir(&child_path)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false)
            {
                fs::remove_dir(&child_path)?;
            }
        } else {
            remove_path(&child_path)?;
        }
    }

    Ok(())
}

fn clear_current_items(spec: &SnapshotSpec) -> Result<(), AppError> {
    for item in &spec.items {
        let preserve = preserve_paths(item);
        if preserve.is_empty() {
            remove_path(&item.path)?;
        } else {
            remove_children_except(&item.path, &preserve, Path::new(""))?;
        }
    }
    Ok(())
}

fn archive_path(spec: &SnapshotSpec, name: &str) -> PathBuf {
    spec.archive_root.join(name)
}

fn existing_items(spec: &SnapshotSpec) -> Vec<SnapshotItem> {
    spec.items
        .iter()
        .filter(|item| {
            let preserve = preserve_paths(item);
            has_archivable_data(&item.path, &preserve, Path::new(""))
        })
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

fn write_archive(
    spec: &SnapshotSpec,
    archive_name: String,
    note: Option<String>,
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
    let mut copied = Vec::new();
    for item in &items {
        let preserve = preserve_paths(item);
        let size = dir_size_recursive(&item.path, &preserve, Path::new(""));
        total_size += size;
        let dst = path.join(item.name);
        copy_item_with_preserve(&item.path, &dst, &preserve).map_err(|e| {
            let _ = fs::remove_dir_all(&path);
            AppError::Archive(format!("copy {} failed: {}", item.name, e))
        })?;
        if !verify_copy(&item.path, &dst, &preserve) {
            let _ = fs::remove_dir_all(&path);
            return Err(AppError::Archive(format!(
                "copy verification failed for {}",
                item.name
            )));
        }
        copied.push(item.name);
    }

    let meta = ArchiveMeta {
        created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        name: archive_name.clone(),
        items: copied.len() as u32,
        total_size,
        size_human: format_size(total_size),
        note,
    };
    fs::write(
        path.join("_meta.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;

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
    let archive = write_archive(spec, archive_name, None)?;
    if spec.clear_current_on_create {
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
        let backup_name = format!("auto-{}", Local::now().format("%Y%m%d-%H%M%S"));
        write_archive(
            spec,
            backup_name,
            Some(format!("Automatic backup before restoring {}", name)),
        )?;
    }

    clear_current_items(spec)?;

    for item in &spec.items {
        let src = path.join(item.name);
        if !src.exists() {
            continue;
        }
        let preserve = preserve_paths(item);
        copy_item_with_preserve(&src, &item.path, &preserve)?;
    }

    Ok(())
}

pub fn delete_profile(spec: &SnapshotSpec, name: &str) -> Result<(), AppError> {
    let path = archive_path(spec, name);
    if !path.exists() {
        return Err(AppError::NotFound(format!("snapshot not found: {}", name)));
    }
    fs::remove_dir_all(path)?;
    Ok(())
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
}
