use crate::agents::{AgentProvider, ProviderRegistry};
use crate::error::AppError;
use crate::models::rule::RuleFile;
use rayon::prelude::*;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub async fn list_rules(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
) -> Result<Vec<RuleFile>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        if source.is_some() {
            return rule_provider(&registry, source.as_deref(), false)?.list_rules();
        }

        let providers: Vec<Arc<dyn AgentProvider>> = registry
            .sources()
            .into_iter()
            .filter(|source| source.available && source.capabilities.rules_read)
            .filter_map(|source| registry.get(&source.id))
            .collect();
        let batches: Result<Vec<Vec<RuleFile>>, AppError> = providers
            .par_iter()
            .map(|provider| provider.list_rules())
            .collect();
        let mut rules: Vec<RuleFile> = batches?.into_iter().flatten().collect();
        rules.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then(a.category.cmp(&b.category))
                .then(a.filename.cmp(&b.filename))
        });
        Ok(rules)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_rule(
    registry: State<'_, ProviderRegistry>,
    category: String,
    filename: String,
    source: Option<String>,
) -> Result<RuleFile, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        crate::safe_path::validate_relative("category", &category)?;
        crate::safe_path::validate_segment("filename", &filename)?;
        rule_provider(&registry, source.as_deref(), false)?.get_rule(&category, &filename)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn toggle_rule(
    registry: State<'_, ProviderRegistry>,
    category: String,
    filename: String,
    enabled: bool,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        crate::safe_path::validate_relative("category", &category)?;
        crate::safe_path::validate_segment("filename", &filename)?;
        rule_provider(&registry, source.as_deref(), true)?
            .toggle_rule(&category, &filename, enabled)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

fn rule_provider(
    registry: &ProviderRegistry,
    source: Option<&str>,
    write: bool,
) -> Result<Arc<dyn AgentProvider>, AppError> {
    if let Some(source) = source {
        let provider = registry
            .get(source)
            .ok_or_else(|| AppError::NotFound(format!("Unknown agent source: {}", source)))?;
        ensure_rule_capability(provider.as_ref(), write)?;
        return Ok(provider);
    }

    for source in registry.sources() {
        let has_capability = if write {
            source.capabilities.rules_write
        } else {
            source.capabilities.rules_read
        };
        if !source.available || !has_capability {
            continue;
        }
        if let Some(provider) = registry.get(&source.id) {
            return Ok(provider);
        }
    }

    Err(AppError::NotFound(if write {
        "No writable rule source".to_string()
    } else {
        "No readable rule source".to_string()
    }))
}

fn ensure_rule_capability(provider: &dyn AgentProvider, write: bool) -> Result<(), AppError> {
    let capabilities = provider.capabilities();
    let ok = if write {
        capabilities.rules_write
    } else {
        capabilities.rules_read
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::Archive(format!(
            "{} 不支持{}规则",
            provider.display_name(),
            if write { "写入" } else { "读取" }
        )))
    }
}
