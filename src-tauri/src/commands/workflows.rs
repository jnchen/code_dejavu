//! Browse user-authored workflow artifacts (skills / commands / plans / tasks).
//!
//! Read-only and aggregated across every provider — each provider contributes whatever it owns
//! (today only Claude, under `~/.claude/{skills,commands,plans,tasks}`). Providers that have no
//! such concept return an empty list via the trait default, so this never branches on "which agent".

use crate::agents::{ProviderRegistry, WorkflowItem};
use crate::error::AppError;
use rayon::prelude::*;
use tauri::State;

#[tauri::command]
pub async fn list_workflows(
    registry: State<'_, ProviderRegistry>,
) -> Result<Vec<WorkflowItem>, AppError> {
    let providers = registry.providers();
    tauri::async_runtime::spawn_blocking(move || {
        let mut items: Vec<WorkflowItem> = providers
            .into_par_iter()
            .flat_map_iter(|provider| provider.list_workflows())
            .collect();
        items.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then(a.kind.cmp(&b.kind))
                .then(a.name.cmp(&b.name))
        });
        items
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))
}

#[tauri::command]
pub async fn read_workflow(
    registry: State<'_, ProviderRegistry>,
    source: String,
    path: String,
) -> Result<String, AppError> {
    let provider = registry
        .get(&source)
        .ok_or_else(|| AppError::NotFound(format!("Unknown agent source: {}", source)))?;
    tauri::async_runtime::spawn_blocking(move || provider.read_workflow(&path))
        .await
        .map_err(|e| AppError::Archive(e.to_string()))?
}
