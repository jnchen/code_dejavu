use crate::agents::{AgentProvider, ProviderRegistry, SourceInfo};
use crate::error::AppError;
use crate::models::session::{PaginatedRecords, SessionSearchHit, SessionSummary, SubagentInfo};
use crate::services::search::{DashboardSummary, IndexStatus, SharedSearchEngine, UsageSummary};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tauri::State;

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredModelPrice {
    pub model: String,
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub matched_model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelPriceRefreshResult {
    pub models: Vec<DiscoveredModelPrice>,
    pub source: String,
    pub used_fallback: bool,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelsResponse {
    #[serde(default)]
    data: Vec<OpenRouterModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    #[serde(default)]
    canonical_slug: Option<String>,
    #[serde(default)]
    pricing: OpenRouterPricing,
}

#[derive(Debug, Default, Deserialize)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: serde_json::Value,
    #[serde(default)]
    completion: serde_json::Value,
}

fn price_per_million(value: &serde_json::Value) -> Option<f64> {
    let value = match value {
        serde_json::Value::String(value) => value.parse::<f64>().ok()?,
        serde_json::Value::Number(value) => value.as_f64()?,
        _ => return None,
    };
    (value.is_finite() && value >= 0.0).then_some(value * 1_000_000.0)
}

fn normalized_model_id(model: &str) -> String {
    let model = model.trim().to_ascii_lowercase();
    let model = model.strip_prefix('~').unwrap_or(&model);
    let model = model.split(':').next().unwrap_or(model);
    model.rsplit('/').next().unwrap_or(model).replace('.', "-")
}

fn catalog_price_for<'a>(
    model: &str,
    catalog: &'a HashMap<String, (&'a str, &'a OpenRouterPricing)>,
) -> Option<(String, &'a OpenRouterPricing)> {
    let normalized = normalized_model_id(model);
    catalog
        .get(&normalized)
        .map(|(matched, pricing)| ((*matched).to_string(), *pricing))
}

/// A small offline safety net, intentionally consulted only when the live catalog is unavailable.
/// Values are USD per one million tokens and use exact normalized ids to avoid guessing prices for
/// provider-specific aliases.
const FALLBACK_MODEL_PRICES: &[(&str, f64, f64)] = &[
    ("claude-fable-5", 10.0, 50.0),
    ("claude-haiku-4-5", 1.0, 5.0),
    ("claude-opus-4-6", 5.0, 25.0),
    ("claude-opus-4-7", 5.0, 25.0),
    ("claude-opus-4-8", 5.0, 25.0),
    ("claude-sonnet-4-6", 3.0, 15.0),
    ("gpt-5-4", 2.5, 15.0),
    ("gpt-5-4-mini", 0.75, 4.5),
    ("gpt-5-4-pro", 30.0, 180.0),
    ("gpt-5-5", 5.0, 30.0),
    ("gpt-5-6-luna", 0.1, 0.6),
    ("gpt-5-6-sol", 5.0, 30.0),
    ("gpt-5-6-terra", 1.0, 6.0),
];

fn fallback_model_price(model: &str) -> Option<DiscoveredModelPrice> {
    let normalized = normalized_model_id(model);
    FALLBACK_MODEL_PRICES
        .iter()
        .find(|(candidate, _, _)| *candidate == normalized)
        .map(|(matched, input, output)| DiscoveredModelPrice {
            model: model.to_string(),
            input: Some(*input),
            output: Some(*output),
            matched_model: Some((*matched).to_string()),
        })
}

fn fallback_models_for(mut models: Vec<String>) -> Vec<DiscoveredModelPrice> {
    models.sort_by_key(|model| model.to_ascii_lowercase());
    models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    let mut seen: HashSet<String> = models
        .iter()
        .map(|model| normalized_model_id(model))
        .collect();
    let mut result: Vec<DiscoveredModelPrice> = models
        .into_iter()
        .map(|model| {
            fallback_model_price(&model).unwrap_or(DiscoveredModelPrice {
                model,
                input: None,
                output: None,
                matched_model: None,
            })
        })
        .collect();
    for (model, input, output) in FALLBACK_MODEL_PRICES {
        if seen.insert((*model).to_string()) {
            result.push(DiscoveredModelPrice {
                model: (*model).to_string(),
                input: Some(*input),
                output: Some(*output),
                matched_model: Some((*model).to_string()),
            });
        }
    }
    result.sort_by_key(|model| model.model.to_ascii_lowercase());
    result
}

async fn fetch_openrouter_catalog() -> Result<Vec<OpenRouterModel>, String> {
    let response = reqwest::Client::builder()
        .user_agent("code-dejavu")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?
        .get("https://openrouter.ai/api/v1/models")
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json::<OpenRouterModelsResponse>()
        .await
        .map_err(|error| error.to_string())?;
    if response.data.is_empty() {
        return Err("OpenRouter returned an empty model catalog".to_string());
    }
    Ok(response.data)
}

#[derive(Clone, Copy)]
enum SessionCapability {
    Read,
    Search,
    Subagents,
}

fn supports(provider: &dyn AgentProvider, capability: SessionCapability) -> bool {
    let capabilities = provider.capabilities();
    match capability {
        SessionCapability::Read => capabilities.sessions_read,
        SessionCapability::Search => capabilities.sessions_search,
        SessionCapability::Subagents => capabilities.sessions_subagents,
    }
}

fn capability_error(provider: &dyn AgentProvider, capability: SessionCapability) -> AppError {
    let action = match capability {
        SessionCapability::Read => "读取会话档案",
        SessionCapability::Search => "搜索会话",
        SessionCapability::Subagents => "子代理档案",
    };
    AppError::Archive(format!("{} 不支持{}", provider.display_name(), action))
}

fn provider_for(
    registry: &ProviderRegistry,
    source: &Option<String>,
    capability: SessionCapability,
) -> Result<Arc<dyn AgentProvider>, AppError> {
    if let Some(source) = source {
        let provider = registry
            .get(source)
            .ok_or_else(|| AppError::NotFound(format!("Unknown agent source: {}", source)))?;
        if supports(provider.as_ref(), capability) {
            return Ok(provider);
        }
        return Err(capability_error(provider.as_ref(), capability));
    }

    for source in registry.sources() {
        if !source.available {
            continue;
        }
        let Some(provider) = registry.get(&source.id) else {
            continue;
        };
        if supports(provider.as_ref(), capability) {
            return Ok(provider);
        }
    }

    Err(AppError::NotFound(match capability {
        SessionCapability::Read => "No readable session source".to_string(),
        SessionCapability::Search => "No searchable session source".to_string(),
        SessionCapability::Subagents => "No subagent source".to_string(),
    }))
}

#[tauri::command]
pub async fn list_sources(
    registry: State<'_, ProviderRegistry>,
) -> Result<Vec<SourceInfo>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || Ok(registry.sources()))
        .await
        .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_index_status(
    engine: State<'_, SharedSearchEngine>,
) -> Result<IndexStatus, AppError> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let guard = engine
            .read()
            .map_err(|e| AppError::Archive(e.to_string()))?;
        Ok(guard.status.clone())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Rebuild the global search index on demand (e.g. a "refresh index" button), so newly created
/// or continued sessions become searchable without restarting the app.
#[tauri::command]
pub async fn rebuild_index(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
) -> Result<IndexStatus, AppError> {
    let providers = registry.providers();
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::services::search::rebuild(&engine, &providers);
        engine
            .read()
            .map(|guard| guard.status.clone())
            .map_err(|e| AppError::Archive(e.to_string()))
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Aggregate token usage across all indexed (current) sessions for the usage panel.
#[tauri::command]
pub async fn usage_summary(
    engine: State<'_, SharedSearchEngine>,
) -> Result<UsageSummary, AppError> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let guard = engine
            .read()
            .map_err(|e| AppError::Archive(e.to_string()))?;
        Ok(guard.usage_summary())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Discover model ids used by local sessions and resolve their current public prices. The local
/// index remains the source of truth for which models are shown; the remote catalog only fills
/// pricing when it exposes an exact normalized model-id match.
#[tauri::command]
pub async fn refresh_model_prices(
    engine: State<'_, SharedSearchEngine>,
) -> Result<ModelPriceRefreshResult, AppError> {
    let engine = engine.inner().clone();
    let mut models = tauri::async_runtime::spawn_blocking(move || {
        let guard = engine
            .read()
            .map_err(|e| AppError::Archive(e.to_string()))?;
        Ok::<Vec<String>, AppError>(guard.discovered_models())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))??;
    models.sort_by_key(|model| model.to_ascii_lowercase());
    models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    let catalog = match fetch_openrouter_catalog().await {
        Ok(catalog) => catalog,
        Err(error) => {
            return Ok(ModelPriceRefreshResult {
                models: fallback_models_for(models),
                source: "Built-in fallback prices".to_string(),
                used_fallback: true,
                error: Some(error),
            });
        }
    };
    let mut entries: HashMap<String, (&str, &OpenRouterPricing)> = HashMap::new();
    for entry in &catalog {
        // Variant prices (for example `:batch`) must not silently replace the standard model.
        if entry.id.contains(':') {
            continue;
        }
        let candidates = [Some(entry.id.as_str()), entry.canonical_slug.as_deref()];
        for candidate in candidates.into_iter().flatten() {
            entries
                .entry(normalized_model_id(candidate))
                .and_modify(|current| {
                    if current.0.starts_with('~') && !entry.id.starts_with('~') {
                        *current = (entry.id.as_str(), &entry.pricing);
                    }
                })
                .or_insert((entry.id.as_str(), &entry.pricing));
        }
    }
    let models = models
        .into_iter()
        .map(|model| {
            let Some((matched_model, pricing)) = catalog_price_for(&model, &entries) else {
                return DiscoveredModelPrice {
                    model,
                    input: None,
                    output: None,
                    matched_model: None,
                };
            };
            DiscoveredModelPrice {
                model,
                input: price_per_million(&pricing.prompt),
                output: price_per_million(&pricing.completion),
                matched_model: Some(matched_model),
            }
        })
        .collect();

    Ok(ModelPriceRefreshResult {
        models,
        source: "OpenRouter model catalog".to_string(),
        used_fallback: false,
        error: None,
    })
}

/// Pre-aggregated dashboard view served entirely from the in-memory index (no disk scan), so the
/// homepage opens instantly instead of re-reading every session file on every visit.
#[tauri::command]
pub async fn dashboard_summary(
    engine: State<'_, SharedSearchEngine>,
) -> Result<DashboardSummary, AppError> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let guard = engine
            .read()
            .map_err(|e| AppError::Archive(e.to_string()))?;
        Ok(guard.dashboard_summary())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Browser's default (no-query) session list served from the in-memory index instead of scanning
/// every session file on disk — the same instant-open treatment the dashboard and search already
/// get. The frontend falls back to `list_sessions` only while the index is still building.
#[tauri::command]
pub async fn browse_sessions(
    engine: State<'_, SharedSearchEngine>,
    source: Option<String>,
    archive_scope: Option<String>,
) -> Result<Vec<SessionSummary>, AppError> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let guard = engine
            .read()
            .map_err(|e| AppError::Archive(e.to_string()))?;
        Ok(guard.browse_summaries(source.as_deref(), archive_scope.as_deref()))
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn list_sessions(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: Option<String>,
) -> Result<Vec<SessionSummary>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        provider_for(&registry, &source, SessionCapability::Read)?.list_sessions(project.as_deref())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn search_sessions(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
    source: Option<String>,
    query: String,
    scopes: Option<Vec<String>>,
    archive_scope: Option<String>,
) -> Result<Vec<SessionSummary>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    // The engine indexes all agents; filter to `source` and the chosen `scopes` (对话/工具/思考).
    let engine = engine.inner().clone();
    let scopes = scopes.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || {
        if source.is_some() {
            provider_for(&registry, &source, SessionCapability::Search)?;
        }
        let guard = engine
            .read()
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let limit = if query.trim().is_empty() { 500 } else { 200 };
        let results = guard.search(
            &query,
            limit,
            &scopes,
            source.as_deref(),
            archive_scope.as_deref(),
        );
        Ok(results
            .into_iter()
            .map(|r| {
                let mut s = r.session;
                // In search mode the card preview should be the matching snippet, not
                // necessarily the session's first prompt; otherwise hits look unrelated.
                s.first_prompt = Some(r.snippet);
                s
            })
            .collect())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Exhaustive full-text search: scan each candidate session's SOURCE file (not the 4000-char index
/// preview) via the provider's in-session search, so precise substrings anywhere in long sessions
/// are found. Slower than the indexed search — meant for an explicit "deep search" action.
#[tauri::command]
pub async fn deep_search(
    registry: State<'_, ProviderRegistry>,
    engine: State<'_, SharedSearchEngine>,
    query: String,
    source: Option<String>,
    archive_scope: Option<String>,
) -> Result<Vec<SessionSummary>, AppError> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let engine = engine.inner().clone();
    let providers: std::collections::HashMap<String, Arc<dyn AgentProvider>> = registry
        .providers()
        .into_iter()
        .map(|p| (p.id().to_string(), p))
        .collect();

    tauri::async_runtime::spawn_blocking(move || {
        use rayon::prelude::*;
        let candidates = {
            let guard = engine
                .read()
                .map_err(|e| AppError::Archive(e.to_string()))?;
            guard.sessions_in_scope(source.as_deref(), archive_scope.as_deref())
        };
        let mut hits: Vec<SessionSummary> = candidates
            .into_par_iter()
            .filter_map(|mut s| {
                let provider = providers.get(&s.source)?;
                let found = provider
                    .search_in_session(&s.project, &s.session_id, &query, s.archive_name.as_deref())
                    .ok()?;
                let first = found.into_iter().next()?;
                s.first_prompt = Some(first.snippet);
                Some(s)
            })
            .collect();
        hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        hits.truncate(300);
        Ok(hits)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
// Args map 1:1 to the frontend `invoke` payload; bundling them into a struct would only add a
// nesting layer on both sides without improving the call site.
#[allow(clippy::too_many_arguments)]
pub async fn get_session_detail(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    byte_offset: u64,
    limit: u32,
    min_level: String,
    archive_name: Option<String>,
) -> Result<PaginatedRecords, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&registry, &source, SessionCapability::Read)?;
        provider
            .session_detail(
                &project,
                &session_id,
                byte_offset,
                limit,
                &min_level,
                archive_name.as_deref(),
            )
            .map(PaginatedRecords::without_terminal_formatting)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_session_tail(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    limit: u32,
    min_level: String,
    archive_name: Option<String>,
) -> Result<PaginatedRecords, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&registry, &source, SessionCapability::Read)?;
        provider
            .session_tail(
                &project,
                &session_id,
                limit,
                &min_level,
                archive_name.as_deref(),
            )
            .map(PaginatedRecords::without_terminal_formatting)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
// Args map 1:1 to the frontend invoke payload, including the exclusive reverse cursor.
#[allow(clippy::too_many_arguments)]
pub async fn get_session_before(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    before_byte_offset: u64,
    limit: u32,
    min_level: String,
    archive_name: Option<String>,
) -> Result<PaginatedRecords, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&registry, &source, SessionCapability::Read)?;
        provider
            .session_before(
                &project,
                &session_id,
                before_byte_offset,
                limit,
                &min_level,
                archive_name.as_deref(),
            )
            .map(PaginatedRecords::without_terminal_formatting)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_session_first_prompt(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    archive_name: Option<String>,
) -> Result<Option<String>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&registry, &source, SessionCapability::Read)?;
        Ok(provider.first_prompt(&project, &session_id, archive_name.as_deref()))
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn list_subagents(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    archive_name: Option<String>,
) -> Result<Vec<SubagentInfo>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = if let Some(source_id) = source.as_deref() {
            let provider = registry.get(source_id).ok_or_else(|| {
                AppError::NotFound(format!("Unknown agent source: {}", source_id))
            })?;
            if !supports(provider.as_ref(), SessionCapability::Subagents) {
                return Ok(Vec::new());
            }
            provider
        } else {
            match provider_for(&registry, &source, SessionCapability::Subagents) {
                Ok(provider) => provider,
                Err(AppError::NotFound(_)) => return Ok(Vec::new()),
                Err(error) => return Err(error),
            }
        };
        provider.list_subagents(&project, &session_id, archive_name.as_deref())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
// Args map 1:1 to the frontend `invoke` payload; bundling them into a struct would only add a
// nesting layer on both sides without improving the call site.
#[allow(clippy::too_many_arguments)]
pub async fn get_subagent_detail(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    agent_id: String,
    byte_offset: u64,
    limit: u32,
    archive_name: Option<String>,
) -> Result<PaginatedRecords, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&registry, &source, SessionCapability::Subagents)?;
        provider
            .subagent_detail(
                &project,
                &session_id,
                &agent_id,
                byte_offset,
                limit,
                archive_name.as_deref(),
            )
            .map(PaginatedRecords::without_terminal_formatting)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn search_in_session(
    registry: State<'_, ProviderRegistry>,
    source: Option<String>,
    project: String,
    session_id: String,
    query: String,
    archive_name: Option<String>,
) -> Result<Vec<SessionSearchHit>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&registry, &source, SessionCapability::Search)?;
        provider.search_in_session(&project, &session_id, &query, archive_name.as_deref())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::{fallback_model_price, fallback_models_for, normalized_model_id};

    #[test]
    fn model_id_normalization_handles_provider_and_version_separators() {
        assert_eq!(
            normalized_model_id("anthropic/claude-fable-5"),
            "claude-fable-5"
        );
        assert_eq!(normalized_model_id("openai/gpt-5.4"), "gpt-5-4");
        assert_eq!(
            normalized_model_id("anthropic/claude-fable-5:batch"),
            "claude-fable-5"
        );
    }

    #[test]
    fn fallback_prices_include_fable_five() {
        let price = fallback_model_price("claude-fable-5").expect("fallback price");
        assert_eq!(price.input, Some(10.0));
        assert_eq!(price.output, Some(50.0));
    }

    #[test]
    fn fallback_catalog_is_available_without_a_local_fable_session() {
        let models = fallback_models_for(Vec::new());
        assert!(models.iter().any(|model| model.model == "claude-fable-5"));
    }
}
