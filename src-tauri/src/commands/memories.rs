use crate::agents::{AgentProvider, ProviderRegistry};
use crate::error::AppError;
use crate::models::memory::{MemoryFile, MemoryFrontmatter, ProjectInfo};
use rayon::prelude::*;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub async fn list_projects(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
) -> Result<Vec<ProjectInfo>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        if source.is_some() {
            return memory_provider(&registry, source.as_deref(), false)?.list_memory_projects();
        }
        let providers: Vec<Arc<dyn AgentProvider>> = registry
            .sources()
            .into_iter()
            .filter(|source| source.available && source.capabilities.memory_read)
            .filter_map(|source| registry.get(&source.id))
            .collect();
        let batches: Result<Vec<Vec<ProjectInfo>>, AppError> = providers
            .into_par_iter()
            .map(|provider| provider.list_memory_projects())
            .collect();
        let mut projects: Vec<ProjectInfo> = batches?.into_iter().flatten().collect();
        projects.sort_by(|a, b| {
            b.last_active
                .as_deref()
                .unwrap_or("")
                .cmp(a.last_active.as_deref().unwrap_or(""))
                .then(a.source.cmp(&b.source))
                .then(a.display_path.cmp(&b.display_path))
        });
        Ok(projects)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn list_memories(
    registry: State<'_, ProviderRegistry>,
    project: String,
    source: Option<String>,
) -> Result<Vec<MemoryFile>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        crate::safe_path::validate_segment("project", &project)?;
        memory_provider(&registry, source.as_deref(), false)?.list_memories(&project)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_memory(
    registry: State<'_, ProviderRegistry>,
    project: String,
    filename: String,
    source: Option<String>,
) -> Result<MemoryFile, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        crate::safe_path::validate_segment("project", &project)?;
        crate::safe_path::validate_segment("filename", &filename)?;
        memory_provider(&registry, source.as_deref(), false)?.get_memory(&project, &filename)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn save_memory(
    registry: State<'_, ProviderRegistry>,
    project: String,
    filename: String,
    frontmatter_data: MemoryFrontmatter,
    content: String,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        crate::safe_path::validate_segment("project", &project)?;
        crate::safe_path::validate_segment("filename", &filename)?;
        let provider = memory_provider(&registry, source.as_deref(), true)?;
        provider.save_memory(&project, &filename, &frontmatter_data, &content)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn delete_memory(
    registry: State<'_, ProviderRegistry>,
    project: String,
    filename: String,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        crate::safe_path::validate_segment("project", &project)?;
        crate::safe_path::validate_segment("filename", &filename)?;
        memory_provider(&registry, source.as_deref(), true)?.delete_memory(&project, &filename)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn create_memory(
    registry: State<'_, ProviderRegistry>,
    project: String,
    filename: String,
    frontmatter_data: MemoryFrontmatter,
    content: String,
    source: Option<String>,
) -> Result<(), AppError> {
    save_memory(
        registry,
        project,
        filename,
        frontmatter_data,
        content,
        source,
    )
    .await
}

fn memory_provider(
    registry: &ProviderRegistry,
    source: Option<&str>,
    write: bool,
) -> Result<Arc<dyn AgentProvider>, AppError> {
    if let Some(source) = source {
        let provider = registry
            .get(source)
            .ok_or_else(|| AppError::NotFound(format!("Unknown agent source: {}", source)))?;
        ensure_memory_capability(provider.as_ref(), write)?;
        return Ok(provider);
    }

    for source in registry.sources() {
        let has_capability = if write {
            source.capabilities.memory_write
        } else {
            source.capabilities.memory_read
        };
        if !source.available || !has_capability {
            continue;
        }
        if let Some(provider) = registry.get(&source.id) {
            return Ok(provider);
        }
    }

    Err(AppError::NotFound(if write {
        "No writable memory source".to_string()
    } else {
        "No readable memory source".to_string()
    }))
}

fn ensure_memory_capability(provider: &dyn AgentProvider, write: bool) -> Result<(), AppError> {
    let capabilities = provider.capabilities();
    let ok = if write {
        capabilities.memory_write
    } else {
        capabilities.memory_read
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::Archive(format!(
            "{} 不支持{}记忆",
            provider.display_name(),
            if write { "写入" } else { "读取" }
        )))
    }
}
