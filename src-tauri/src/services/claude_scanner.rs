use crate::error::AppError;
use crate::hosts::Host;
use crate::models::memory::ProjectInfo;
use crate::paths::ClaudePaths;
use rayon::prelude::*;
use std::fs;

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn count_files_with_ext(path: &std::path::Path, ext: &str) -> u32 {
    if !path.exists() {
        return 0;
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == ext))
        .count() as u32
}

pub fn list_projects(paths: &ClaudePaths, host: &Host) -> Result<Vec<ProjectInfo>, AppError> {
    let mut projects = Vec::new();
    if !paths.projects_dir.exists() {
        return Ok(projects);
    }

    let project_dirs: Vec<(String, std::path::PathBuf)> = fs::read_dir(&paths.projects_dir)?
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            )
        })
        .collect();
    projects = project_dirs
        .into_par_iter()
        .map(|(slug, proj_path)| {
            let mem_dir = proj_path.join("memory");
            let memory_count = if mem_dir.exists() {
                count_files_with_ext(&mem_dir, "md")
            } else {
                0
            };
            let session_count = count_files_with_ext(&proj_path, "jsonl");
            let last_active = latest_modified_time(&proj_path, "jsonl");
            ProjectInfo {
                source: "claude".to_string(),
                source_display_name: "Claude Code".to_string(),
                display_path: host.decode_project_slug(&slug),
                slug,
                memory_count,
                session_count,
                last_active,
            }
        })
        .collect();

    projects.sort_by(|a, b| {
        b.last_active
            .as_deref()
            .unwrap_or("")
            .cmp(a.last_active.as_deref().unwrap_or(""))
    });
    Ok(projects)
}

fn latest_modified_time(dir: &std::path::Path, ext: &str) -> Option<String> {
    let mut latest: Option<std::time::SystemTime> = None;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == ext) && path.is_file() {
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(modified) = meta.modified() {
                        if latest.is_none_or(|l| modified > l) {
                            latest = Some(modified);
                        }
                    }
                }
            }
        }
    }
    latest.and_then(|t| {
        t.duration_since(std::time::UNIX_EPOCH).ok().and_then(|d| {
            chrono::DateTime::from_timestamp(d.as_secs() as i64, 0).map(
                |dt: chrono::DateTime<chrono::Utc>| {
                    let local: chrono::DateTime<chrono::Local> = dt.into();
                    local.format("%Y-%m-%d %H:%M").to_string()
                },
            )
        })
    })
}
