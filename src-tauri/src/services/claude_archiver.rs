use crate::error::AppError;
use crate::models::profile::{ArchiveMeta, ProfileArchive};
use crate::paths::ClaudePaths;
use crate::services::claude_scanner::format_size;
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

fn dir_size_recursive(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn remove_path(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else if path.is_file() {
        let _ = fs::remove_file(path);
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn copy_item(src: &Path, dst: &Path) -> Result<(), AppError> {
    if src.is_dir() {
        copy_dir_recursive(src, dst)?;
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn verify_copy(src: &Path, dst: &Path) -> bool {
    if !dst.exists() {
        return false;
    }
    if src.is_file() && dst.is_file() {
        let src_len = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let dst_len = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        return src_len == dst_len;
    }
    if src.is_dir() && dst.is_dir() {
        let src_count = walkdir::WalkDir::new(src)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count();
        let dst_count = walkdir::WalkDir::new(dst)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count();
        return src_count == dst_count;
    }
    false
}

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
    fs::create_dir_all(&archive_path)?;

    // Phase 1: 复制所有数据到归档目录
    let mut copied_items = Vec::new();
    let mut total_size = 0u64;

    for item in USER_DATA_ITEMS {
        let src = paths.claude_dir.join(item);
        if src.exists() {
            let size = dir_size_recursive(&src);
            total_size += size;
            let dst = archive_path.join(item);
            copy_item(&src, &dst).map_err(|e| {
                // 复制失败，清理已复制的部分归档
                let _ = fs::remove_dir_all(&archive_path);
                AppError::Archive(format!("复制 {} 失败: {}，归档已回滚", item, e))
            })?;
            // 验证复制完整性
            if !verify_copy(&src, &dst) {
                let _ = fs::remove_dir_all(&archive_path);
                return Err(AppError::Archive(format!(
                    "复制 {} 后验证失败（文件数或大小不匹配），归档已回滚",
                    item
                )));
            }
            copied_items.push(item);
        }
    }

    // Phase 2: 写 meta（在删除原文件之前，确保归档完整）
    let meta = ArchiveMeta {
        created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        name: archive_name.clone(),
        items: copied_items.len() as u32,
        total_size,
        size_human: format_size(total_size),
        note: None,
    };
    fs::write(
        archive_path.join("_meta.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;

    // Phase 3: 归档验证通过后，才删除原文件
    for item in &copied_items {
        let src = paths.claude_dir.join(item);
        remove_path(&src);
    }

    // Phase 4: 清理临时缓存
    for item in EPHEMERAL_ITEMS {
        let p = paths.claude_dir.join(item);
        remove_path(&p);
    }

    Ok(ProfileArchive {
        source: "claude".to_string(),
        source_display_name: "Claude Code".to_string(),
        name: archive_name,
        created: meta.created,
        items: copied_items.len() as u32,
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

    // Phase 1: 如果当前有数据，先归档保护
    let has_existing = USER_DATA_ITEMS
        .iter()
        .any(|item| paths.claude_dir.join(item).exists());
    if has_existing {
        let backup_name = format!("auto-{}", Local::now().format("%Y%m%d-%H%M%S"));
        let backup_path = paths.archive_root.join(&backup_name);
        fs::create_dir_all(&backup_path)?;

        let mut backup_count = 0u32;
        let mut backup_size = 0u64;

        for item in USER_DATA_ITEMS {
            let src = paths.claude_dir.join(item);
            if src.exists() {
                let size = dir_size_recursive(&src);
                backup_size += size;
                let dst = backup_path.join(item);
                copy_item(&src, &dst).map_err(|e| {
                    AppError::Archive(format!(
                        "自动备份 {} 失败: {}，恢复操作已中止（原数据未动）",
                        item, e
                    ))
                })?;
                backup_count += 1;
            }
        }

        let backup_meta = ArchiveMeta {
            created: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            name: backup_name,
            items: backup_count,
            total_size: backup_size,
            size_human: format_size(backup_size),
            note: Some(format!("恢复 {} 前的自动备份", name)),
        };
        fs::write(
            backup_path.join("_meta.json"),
            serde_json::to_string_pretty(&backup_meta)?,
        )?;

        // 备份成功后才删除当前数据
        for item in USER_DATA_ITEMS {
            let src = paths.claude_dir.join(item);
            remove_path(&src);
        }
    }

    // Phase 2: 清理临时缓存
    for item in EPHEMERAL_ITEMS {
        remove_path(&paths.claude_dir.join(item));
    }

    // Phase 3: 从归档复制回来（不动归档本身）
    for entry in fs::read_dir(&archive_path)?.flatten() {
        let filename = entry.file_name().to_string_lossy().to_string();
        if filename == "_meta.json" {
            continue;
        }
        let dst = paths.claude_dir.join(&filename);
        let src = entry.path();
        copy_item(&src, &dst)?;
    }

    Ok(())
}

pub fn delete_profile(paths: &ClaudePaths, name: &str) -> Result<(), AppError> {
    let archive_path = paths.archive_root.join(name);
    if !archive_path.exists() {
        return Err(AppError::NotFound(format!("归档不存在: {}", name)));
    }
    fs::remove_dir_all(&archive_path)?;
    Ok(())
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
