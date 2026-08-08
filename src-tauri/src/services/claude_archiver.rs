use crate::error::AppError;
use crate::models::profile::{ArchiveMeta, ProfileArchive};
use crate::paths::ClaudePaths;
use crate::services::claude_scanner::format_size;
use crate::services::fsops;
use chrono::Local;
use std::fs;
use std::path::Path;

const USER_DATA_ITEMS: &[&str] = &[
    "CLAUDE.md",
    "rules",
    "settings.json",
    "projects",
    "session-data",
    "plans",
    "skills",
    "tasks",
    "history.jsonl",
    "backups",
    "cost-tracker.log",
    "cost_daily.json",
    "bash-commands.log",
];

const EPHEMERAL_ITEMS: &[&str] = &[
    "cache",
    "daemon",
    "debug",
    "ide",
    "metrics",
    "paste-cache",
    "plugins",
    "session-env",
    "sessions",
    "shell-snapshots",
    "telemetry",
    "file-history",
    ".last-cleanup",
    ".last-update-result.json",
    "mcp-health-cache.json",
    "mcp-needs-auth-cache.json",
    "package-manager.json",
    "stats-cache.json",
    "statusline_debug.json",
];

pub fn list_profiles(paths: &ClaudePaths) -> Result<Vec<ProfileArchive>, AppError> {
    let mut profiles = Vec::new();
    if !paths.archive_root.exists() {
        return Ok(profiles);
    }
    for entry in fs::read_dir(&paths.archive_root)?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let meta_path = entry.path().join("_meta.json");
        if let Ok(content) = fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<ArchiveMeta>(&content) {
                profiles.push(ProfileArchive {
                    source: "claude".to_string(),
                    source_display_name: "Claude Code".to_string(),
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
            source: "claude".to_string(),
            source_display_name: "Claude Code".to_string(),
            is_auto: name.starts_with("auto-"),
            created: String::new(),
            items: 0,
            total_size: 0,
            size_human: "?".into(),
            note: None,
            name,
        });
    }
    profiles.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(profiles)
}

/// Put items already moved into `archive_path` back where they came from. Used when a later item
/// fails: the archive is incomplete, and because archiving *moves*, the live directory is the only
/// other place that data can be — so it has to be restored before the archive is discarded.
fn rollback_moved(claude_dir: &Path, archive_path: &Path, moved: &[&str]) {
    for item in moved {
        let from = archive_path.join(item);
        let to = claude_dir.join(item);
        if !from.exists() || to.exists() {
            continue;
        }
        if fs::rename(&from, &to).is_err() {
            let _ = fsops::copy_tree(&from, &to, &[]);
        }
    }
}

/// Move every user-data item into `archive_path`, leaving the live directory empty of them.
/// Returns the archived items and their total size, or restores everything and fails.
fn move_items_into(
    claude_dir: &Path,
    archive_path: &Path,
) -> Result<(Vec<&'static str>, u64), AppError> {
    let mut moved: Vec<&'static str> = Vec::new();
    let mut total_size = 0u64;

    for item in USER_DATA_ITEMS {
        let src = claude_dir.join(item);
        if !src.exists() {
            continue;
        }
        match fsops::move_tree(&src, &archive_path.join(item), &[]) {
            Ok(transfer) => {
                total_size += transfer.bytes;
                moved.push(item);
            }
            Err(err) => {
                rollback_moved(claude_dir, archive_path, &moved);
                let _ = fsops::remove_tree(archive_path);
                return Err(AppError::Archive(format!(
                    "归档 {} 失败: {}，已回滚",
                    item, err
                )));
            }
        }
    }

    Ok((moved, total_size))
}

pub fn create_profile(
    paths: &ClaudePaths,
    label: Option<String>,
) -> Result<ProfileArchive, AppError> {
    let has_data = USER_DATA_ITEMS
        .iter()
        .any(|item| paths.claude_dir.join(item).exists());
    if !has_data {
        return Err(AppError::Archive(
            "没有可归档的数据——Profile 已是干净状态".into(),
        ));
    }

    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let archive_name = match &label {
        Some(l) => format!("{}-{}", timestamp, l),
        None => timestamp,
    };
    let archive_path = paths.archive_root.join(&archive_name);
    if archive_path.exists() {
        return Err(AppError::Archive(format!("归档已存在: {}", archive_name)));
    }
    fs::create_dir_all(&archive_path)?;

    // Archiving relocates the data rather than duplicating it: the live items are meant to end up
    // gone either way, and a same-volume rename is both instant and atomic, so there is no window
    // where a half-copied archive can be mistaken for a complete one.
    let (archived_items, total_size) = move_items_into(&paths.claude_dir, &archive_path)?;

    let meta = ArchiveMeta {
        created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        name: archive_name.clone(),
        items: archived_items.len() as u32,
        total_size,
        size_human: format_size(total_size),
        note: None,
    };
    if let Err(err) = fs::write(
        archive_path.join("_meta.json"),
        serde_json::to_string_pretty(&meta)?,
    ) {
        rollback_moved(&paths.claude_dir, &archive_path, &archived_items);
        let _ = fsops::remove_tree(&archive_path);
        return Err(AppError::Archive(format!(
            "写入归档元数据失败: {}，已回滚",
            err
        )));
    }

    for item in EPHEMERAL_ITEMS {
        let _ = fsops::remove_tree(&paths.claude_dir.join(item));
    }

    Ok(ProfileArchive {
        source: "claude".to_string(),
        source_display_name: "Claude Code".to_string(),
        name: archive_name,
        created: meta.created,
        items: meta.items,
        total_size,
        size_human: meta.size_human,
        note: None,
        is_auto: false,
    })
}

pub fn restore_profile(paths: &ClaudePaths, name: &str) -> Result<(), AppError> {
    let archive_path = paths.archive_root.join(name);
    if !archive_path.exists() {
        return Err(AppError::NotFound(format!("归档不存在: {}", name)));
    }

    // Phase 1: park whatever is live in an auto-backup archive before overwriting it.
    let has_existing = USER_DATA_ITEMS
        .iter()
        .any(|item| paths.claude_dir.join(item).exists());
    if has_existing {
        let backup_name = format!("auto-{}", Local::now().format("%Y%m%d-%H%M%S"));
        let backup_path = paths.archive_root.join(&backup_name);
        fs::create_dir_all(&backup_path)?;

        let (backed_up, backup_size) = move_items_into(&paths.claude_dir, &backup_path)
            .map_err(|err| AppError::Archive(format!("{}，恢复操作已中止", err)))?;

        let backup_meta = ArchiveMeta {
            created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            name: backup_name,
            items: backed_up.len() as u32,
            total_size: backup_size,
            size_human: format_size(backup_size),
            note: Some(format!("恢复 {} 前的自动备份", name)),
        };
        fs::write(
            backup_path.join("_meta.json"),
            serde_json::to_string_pretty(&backup_meta)?,
        )?;
    }

    // Phase 2: 清理临时缓存
    for item in EPHEMERAL_ITEMS {
        let _ = fsops::remove_tree(&paths.claude_dir.join(item));
    }

    // Phase 3: copy — never move — out of the archive, so the archive stays intact afterwards.
    for entry in fs::read_dir(&archive_path)?.flatten() {
        let filename = entry.file_name().to_string_lossy().to_string();
        if filename == "_meta.json" {
            continue;
        }
        fsops::copy_tree(&entry.path(), &paths.claude_dir.join(&filename), &[])?;
    }

    Ok(())
}

pub fn delete_profile(paths: &ClaudePaths, name: &str) -> Result<(), AppError> {
    let archive_path = paths.archive_root.join(name);
    if !archive_path.exists() {
        return Err(AppError::NotFound(format!("归档不存在: {}", name)));
    }
    fsops::remove_tree(&archive_path)
}

pub fn rename_profile(paths: &ClaudePaths, old: &str, new: &str) -> Result<(), AppError> {
    let old_path = paths.archive_root.join(old);
    let new_path = paths.archive_root.join(new);
    if !old_path.exists() {
        return Err(AppError::NotFound(format!("归档不存在: {}", old)));
    }
    if new_path.exists() {
        return Err(AppError::Archive(format!("归档已存在: {}", new)));
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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempHome {
        path: PathBuf,
    }

    impl TempHome {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "dejavu-claude-archiver-{}-{}-{}",
                name,
                std::process::id(),
                nonce
            ));
            fs::create_dir_all(&path).expect("temp home");
            Self { path }
        }

        fn paths(&self) -> ClaudePaths {
            ClaudePaths::for_home(&self.path)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }

    fn seed(paths: &ClaudePaths) {
        write(&paths.claude_md, "# global");
        write(
            &paths.projects_dir.join("C--demo").join("a.jsonl"),
            "{\"x\":1}",
        );
        write(&paths.claude_dir.join("cache").join("junk.bin"), "junk");
    }

    #[test]
    fn create_moves_user_data_into_the_archive_and_clears_the_live_dir() {
        let home = TempHome::new("create");
        let paths = home.paths();
        seed(&paths);

        let archive = create_profile(&paths, Some("demo".to_string())).expect("create");
        let archived = paths.archive_root.join(&archive.name);

        assert_eq!(archive.items, 2);
        assert_eq!(archive.total_size, 8 + 7);
        assert_eq!(
            fs::read_to_string(archived.join("projects").join("C--demo").join("a.jsonl"))
                .expect("archived session"),
            "{\"x\":1}"
        );
        assert!(!paths.projects_dir.exists());
        assert!(!paths.claude_md.exists());
        // Ephemeral caches are dropped, not archived.
        assert!(!paths.claude_dir.join("cache").exists());
        assert!(!archived.join("cache").exists());
    }

    #[test]
    fn restore_auto_backs_up_current_data_and_keeps_the_source_archive_intact() {
        let home = TempHome::new("restore");
        let paths = home.paths();
        seed(&paths);
        let archive = create_profile(&paths, None).expect("create");

        write(&paths.claude_md, "# replaced");
        restore_profile(&paths, &archive.name).expect("restore");

        assert_eq!(
            fs::read_to_string(&paths.claude_md).expect("restored"),
            "# global"
        );
        assert!(paths.projects_dir.join("C--demo").join("a.jsonl").exists());
        // The restored-from archive must survive the restore untouched.
        assert!(paths
            .archive_root
            .join(&archive.name)
            .join("projects")
            .join("C--demo")
            .join("a.jsonl")
            .exists());
        // ...and the pre-restore state is recoverable from the auto backup.
        let autos: Vec<_> = list_profiles(&paths)
            .expect("list")
            .into_iter()
            .filter(|profile| profile.is_auto)
            .collect();
        assert_eq!(autos.len(), 1);
        assert_eq!(
            fs::read_to_string(paths.archive_root.join(&autos[0].name).join("CLAUDE.md"))
                .expect("backed up"),
            "# replaced"
        );
    }
}
