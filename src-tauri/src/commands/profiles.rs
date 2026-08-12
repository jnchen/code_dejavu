use crate::agents::{AgentProvider, ProviderRegistry};
use crate::error::AppError;
use crate::models::profile::ProfileArchive;
use crate::services::search::SharedSearchEngine;
use rayon::prelude::*;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub async fn list_profiles(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
) -> Result<Vec<ProfileArchive>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        if source.is_some() {
            return archive_provider(&registry, source.as_deref(), false)?.list_profiles();
        }
        let providers: Vec<Arc<dyn AgentProvider>> = registry
            .sources()
            .into_iter()
            .filter(|source| source.available && source.capabilities.archive_read)
            .filter_map(|source| registry.get(&source.id))
            .collect();
        let batches: Result<Vec<Vec<ProfileArchive>>, AppError> = providers
            .into_par_iter()
            .map(|provider| provider.list_profiles())
            .collect();
        let mut profiles: Vec<ProfileArchive> = batches?.into_iter().flatten().collect();
        profiles.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then(b.name.cmp(&a.name))
                .then(b.created.cmp(&a.created))
        });
        Ok(profiles)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn create_profile(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
    name: Option<String>,
    source: Option<String>,
) -> Result<ProfileArchive, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    let providers = registry.providers();
    let engine = engine.inner().clone();
    let profile =
        tauri::async_runtime::spawn_blocking(move || -> Result<ProfileArchive, AppError> {
            let profile =
                archive_provider(&registry, source.as_deref(), true)?.create_profile(name)?;
            // Snapshot creation moves the session store, so the old index would otherwise keep
            // returning live summaries whose database no longer exists. Reconcile it before the
            // command returns; the background watcher remains a fallback for external changes.
            crate::services::search::rebuild(&engine, &providers);
            Ok(profile)
        })
        .await
        .map_err(|e| AppError::Archive(e.to_string()))??;
    crate::services::jsonl::clear_page_cache();
    Ok(profile)
}

#[tauri::command]
pub async fn restore_profile(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
    name: String,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    let providers = registry.providers();
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), AppError> {
        archive_provider(&registry, source.as_deref(), true)?.restore_profile(&name)?;
        crate::services::search::rebuild(&engine, &providers);
        Ok(())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))??;
    crate::services::jsonl::clear_page_cache();
    Ok(())
}

#[tauri::command]
pub async fn delete_profile(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
    name: String,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    let providers = registry.providers();
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), AppError> {
        archive_provider(&registry, source.as_deref(), true)?.delete_profile(&name)?;
        crate::services::search::rebuild(&engine, &providers);
        Ok(())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn rename_profile(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
    old_name: String,
    new_name: String,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    let providers = registry.providers();
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), AppError> {
        archive_provider(&registry, source.as_deref(), true)?
            .rename_profile(&old_name, &new_name)?;
        crate::services::search::rebuild(&engine, &providers);
        Ok(())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

fn archive_provider(
    registry: &ProviderRegistry,
    source: Option<&str>,
    write: bool,
) -> Result<Arc<dyn AgentProvider>, AppError> {
    if let Some(source) = source {
        let provider = registry
            .get(source)
            .ok_or_else(|| AppError::NotFound(format!("Unknown agent source: {}", source)))?;
        ensure_archive_capability(provider.as_ref(), write)?;
        return Ok(provider);
    }

    for source in registry.sources() {
        let has_capability = if write {
            source.capabilities.archive_write
        } else {
            source.capabilities.archive_read
        };
        if !source.available || !has_capability {
            continue;
        }
        if let Some(provider) = registry.get(&source.id) {
            return Ok(provider);
        }
    }

    Err(AppError::NotFound(if write {
        "No writable snapshot source".to_string()
    } else {
        "No readable snapshot source".to_string()
    }))
}

fn ensure_archive_capability(provider: &dyn AgentProvider, write: bool) -> Result<(), AppError> {
    let capabilities = provider.capabilities();
    let ok = if write {
        capabilities.archive_write
    } else {
        capabilities.archive_read
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::Archive(format!(
            "{} 不支持{}快照",
            provider.display_name(),
            if write { "写入" } else { "读取" }
        )))
    }
}
