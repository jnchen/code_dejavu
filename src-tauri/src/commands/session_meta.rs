//! App-local, per-session metadata (favourite / pinned / tags / note).
//!
//! This is intentionally stored OUTSIDE any agent's data directory — it lives under the app's own
//! data dir, so organising sessions never mutates real `~/.claude` (or Codex/OpenCode) files.
//! Keyed by "<source>::<session_id>".

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub note: String,
}

impl SessionMeta {
    fn is_empty(&self) -> bool {
        !self.favorite && !self.pinned && self.tags.is_empty() && self.note.trim().is_empty()
    }
}

type MetaMap = HashMap<String, SessionMeta>;

fn meta_path() -> PathBuf {
    crate::paths::app_data_dir().join("session_meta.json")
}

fn load_all() -> MetaMap {
    let path = meta_path();
    if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        MetaMap::new()
    }
}

fn save_all(map: &MetaMap) -> Result<(), AppError> {
    let path = meta_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(map)?)?;
    Ok(())
}

#[tauri::command]
pub async fn list_session_meta() -> Result<MetaMap, AppError> {
    tauri::async_runtime::spawn_blocking(load_all)
        .await
        .map_err(|e| AppError::Archive(e.to_string()))
}

#[tauri::command]
pub async fn set_session_meta(key: String, meta: SessionMeta) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || set_session_meta_blocking(key, meta))
        .await
        .map_err(|e| AppError::Archive(e.to_string()))?
}

fn set_session_meta_blocking(key: String, mut meta: SessionMeta) -> Result<(), AppError> {
    // Normalise tags: trim, drop empties, de-dup (preserve order).
    let mut seen = HashSet::new();
    meta.tags = std::mem::take(&mut meta.tags)
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();

    let mut map = load_all();
    if meta.is_empty() {
        // Don't persist all-default rows — keeps the file small and "no metadata" unambiguous.
        map.remove(&key);
    } else {
        map.insert(key, meta);
    }
    save_all(&map)?;
    Ok(())
}
