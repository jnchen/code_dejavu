//! Coding-agent provider abstraction.
//!
//! Every supported agent (Claude Code, Codex, …) is an [`AgentProvider`]. The session-reading
//! infrastructure that is genuinely hard to get right — seekable pagination, byte-offset scroll
//! anchoring and the append-only page cache — lives ONCE in `services::jsonl` and is generic over
//! [`LineParser`]; each agent supplies only its per-line parsing. Everything agent-specific
//! (paths, project grouping, record→`SessionRecord` mapping) is hidden behind the provider so the
//! commands layer never branches on "which agent".

use crate::error::AppError;
use crate::models::memory::{MemoryFile, MemoryFrontmatter, ProjectInfo};
use crate::models::profile::ProfileArchive;
use crate::models::rule::RuleFile;
use crate::models::session::{
    PaginatedRecords, SessionModelInfo, SessionRecord, SessionSearchHit, SessionSummary,
    SubagentInfo,
};
use crate::paths::ClaudePaths;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub mod claude;
pub mod codex;
pub mod multi_host;
pub mod opencode;

/// Short metadata scans (session identity, project roots) serve page loads and must not queue
/// behind the global indexing workers. A separate, bounded pool also lets providers coalesce a
/// scan under their cache mutex without blocking nested Rayon work.
pub(crate) fn metadata_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(|count| count.get().saturating_sub(1).clamp(1, 4))
            .unwrap_or(1);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("dejavu-meta-{index}"))
            .build()
            .expect("build metadata thread pool")
    })
}

/// The ONLY agent-specific part of session reading. The pager, anchoring and cache are generic
/// over this trait, so a new agent reuses all of that by implementing just these four methods.
pub trait LineParser {
    /// Reset all cross-line parser state before reparsing a larger reverse window.
    fn reset(&mut self);
    /// Convert one raw JSON record into 0+ display records.
    fn push(&mut self, val: &serde_json::Value, min_level: &str) -> Vec<SessionRecord>;
    /// Flush any text accumulated across streamed chunks at EOF / page end.
    fn flush(&mut self, min_level: &str) -> Vec<SessionRecord>;
    /// The grouping key for a record (records sharing it were one model turn → parallel batch).
    fn group_of(&self, val: &serde_json::Value) -> Option<String>;
    /// Cheap raw-line check: true ⇒ the line is certainly non-displayable at `min_level`, so the
    /// (potentially multi-MB) serde parse can be skipped entirely. Must never drop shown content.
    fn skippable(&self, line: &str, min_level: &str) -> bool;
}

/// What an agent supports, so the UI hides inapplicable features instead of branching on the id.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub sessions_read: bool,
    pub sessions_search: bool,
    pub sessions_resume: bool,
    pub sessions_subagents: bool,
    pub rules_read: bool,
    pub rules_write: bool,
    pub memory_read: bool,
    pub memory_write: bool,
    pub instructions_read: bool,
    pub instructions_write: bool,
    pub archive_read: bool,
    pub archive_write: bool,
    /// "json" | "jsonc" | "toml"
    pub config_format: &'static str,
}

/// A searchable text chunk tagged with its kind, so the index can filter by scope.
/// `kind`: "content" (对话) | "tool" (工具 I/O) | "reasoning" (思考).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexText {
    pub kind: String,
    pub text: String,
}

// The fast index is deliberately a bounded preview index. Exhaustive recall is provided by the
// explicit deep-search command, which scans the source session. Bounding at provider parse time is
// important: otherwise an IndexBatch can retain every multi-megabyte tool result before the search
// service gets a chance to trim it.
const FAST_INDEX_TEXT_BYTE_BUDGETS: [usize; 3] = [64 * 1024, 32 * 1024, 32 * 1024];
const FAST_INDEX_TEXT_CHUNK_BUDGETS: [usize; 3] = [128, 64, 64];

fn fast_index_kind(kind: &str) -> usize {
    match kind {
        "tool" => 1,
        "reasoning" => 2,
        _ => 0,
    }
}

/// Streaming, per-session collector used by providers while parsing source data. It bounds both
/// retained bytes and chunk count for every search scope, so neither one giant record nor millions
/// of tiny records can make a cold index build consume unbounded memory.
pub(crate) struct FastIndexTextCollector {
    texts: Vec<IndexText>,
    remaining_bytes: [usize; 3],
    remaining_chunks: [usize; 3],
}

impl Default for FastIndexTextCollector {
    fn default() -> Self {
        Self {
            texts: Vec::new(),
            remaining_bytes: FAST_INDEX_TEXT_BYTE_BUDGETS,
            remaining_chunks: FAST_INDEX_TEXT_CHUNK_BUDGETS,
        }
    }
}

impl FastIndexTextCollector {
    pub(crate) fn push(&mut self, kind: &str, mut text: String) {
        if text.trim().is_empty() {
            return;
        }
        let scope = fast_index_kind(kind);
        let byte_budget = self.remaining_bytes[scope];
        if byte_budget == 0 || self.remaining_chunks[scope] == 0 {
            return;
        }
        if text.len() > byte_budget {
            let mut end = byte_budget;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        if text.trim().is_empty() {
            return;
        }
        self.remaining_bytes[scope] -= text.len();
        self.remaining_chunks[scope] -= 1;
        self.texts.push(IndexText {
            kind: kind.to_string(),
            text,
        });
    }

    pub(crate) fn into_texts(self) -> Vec<IndexText> {
        self.texts
    }
}

/// Defensive boundary for providers (and tests) that construct an IndexDoc directly instead of
/// using FastIndexTextCollector while parsing.
pub(crate) fn bound_fast_index_texts(texts: Vec<IndexText>) -> Vec<IndexText> {
    let mut collector = FastIndexTextCollector::default();
    for IndexText { kind, text } in texts {
        collector.push(&kind, text);
    }
    collector.into_texts()
}

/// Per-session token usage, summed while a provider scans the session for indexing.
/// All fields default to 0, so a provider that can't (yet) parse usage simply reports zero.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub total_tokens: u64,
}

/// One session's searchable content, contributed by a provider to the global search index.
pub struct IndexDoc {
    pub source: String,
    pub session_id: String,
    pub project: String,
    pub project_path: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub agent_title: Option<String>,
    /// Backward-compatible alias for `updated_at`.
    pub timestamp: Option<String>,
    pub file_size_bytes: u64,
    pub subagent_count: u32,
    pub archive_name: Option<String>,
    pub first_prompt: Option<String>,
    pub model_contexts: Vec<SessionModelInfo>,
    /// Kind-tagged conversation/tool/reasoning text — NOT system prompts or raw JSON.
    pub texts: Vec<IndexText>,
    /// Summed token usage for this session (0 if the provider doesn't parse usage).
    pub tokens: TokenUsage,
    /// Stable per-document identity within the provider (e.g. the session file path). Used as the
    /// incremental-index cache key. Empty is allowed for providers that don't support incremental.
    pub key: String,
    /// Cheap content version that changes iff the document changed (e.g. "size:mtime_secs"). Lets
    /// the engine skip re-parsing unchanged documents.
    pub version: String,
}

/// A cheap (no content parse) manifest entry for incremental indexing: a stable key plus a version
/// string that changes iff the underlying content changed.
pub struct IndexManifestEntry {
    pub key: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub display_name: String,
    /// Whether this agent's data root exists on disk (else the UI greys it out).
    pub available: bool,
    pub capabilities: Capabilities,
    /// Non-native machines this source also reads (e.g. `["WSL:Ubuntu"]`). Empty on a plain
    /// install; the UI uses it to offer a host choice where one is genuinely needed.
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Output of a provider's index scan: the documents plus how many sessions/files could not be read
/// or parsed. Surfacing `failed` lets the UI say "N files couldn't be parsed" instead of silently
/// dropping them.
#[derive(Default)]
pub struct IndexBatch {
    pub docs: Vec<IndexDoc>,
    pub failed: usize,
}

/// A user-authored workflow artifact (skill / command / plan / task) surfaced for browsing.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowItem {
    pub source: String,
    pub source_display_name: String,
    /// "skill" | "command" | "plan" | "task"
    pub kind: String,
    pub name: String,
    /// "global" | "project"
    pub scope: String,
    pub path: String,
    pub description: String,
    pub size_bytes: u64,
}

/// A provider-declared instruction/config file that may be shown on the Instructions page.
#[derive(Debug, Clone)]
pub struct InstructionCandidate {
    pub title: String,
    pub scope: &'static str,
    pub kind: &'static str,
    /// File path or provider-local handle. File-backed providers can use the default read/write
    /// methods; non-file providers should override instruction content methods below.
    pub path: PathBuf,
    pub editable: bool,
    pub include_missing: bool,
    pub exists: Option<bool>,
    pub size_bytes: Option<u64>,
    pub description: String,
}

pub fn quote_command_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

/// One coding-agent backend. Session-domain methods for now; config/memory/rules/archive join the
/// trait as each agent grows them (kept lean so the first two providers stay honest).
pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// True if this agent's data exists on disk.
    fn available(&self) -> bool;

    /// Labels for the non-native machines this source also covers. Only a multi-host provider has
    /// any, so the default is "just this machine".
    fn hosts(&self) -> Vec<String> {
        Vec::new()
    }

    /// Filesystem roots whose changes should invalidate the search index (for auto-refresh).
    /// Session-backed providers point this at their session store(s); others may leave it empty.
    fn data_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn list_sessions(&self, _project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
        Ok(Vec::new())
    }
    /// Searchable documents for the global jieba index (one per session, with conversation text),
    /// plus a count of sessions/files that failed to parse. Default empty, so partial providers can
    /// opt in when their session format is understood.
    fn index_documents(&self) -> IndexBatch {
        IndexBatch::default()
    }
    /// Cheap listing of (key, version) for every indexable document WITHOUT parsing content. An
    /// empty result means this provider doesn't support incremental indexing, so the engine treats
    /// each refresh as a full reparse via [`index_documents`](Self::index_documents).
    fn index_manifest(&self) -> Vec<IndexManifestEntry> {
        Vec::new()
    }
    /// Parse ONLY the documents whose key is in `only` (used for incremental refresh). The default
    /// parses everything then filters — correct but not faster; file-based providers override this
    /// to read just the changed files.
    fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
        let mut batch = self.index_documents();
        batch.docs.retain(|doc| only.contains(&doc.key));
        batch
    }
    /// User-authored workflow artifacts (skills/commands/plans/tasks) for the Workflows browser.
    /// Default empty so providers without this concept opt out instead of branching on the id.
    fn list_workflows(&self) -> Vec<WorkflowItem> {
        Vec::new()
    }
    /// Read one workflow artifact's content. Implementors MUST verify `path` is within their own
    /// workflow roots before reading, so this can't be used to read arbitrary files.
    fn read_workflow(&self, _path: &str) -> Result<String, AppError> {
        Err(AppError::NotFound("workflow not found".to_string()))
    }
    /// Global instruction/config files owned by this provider.
    fn global_instruction_candidates(&self) -> Vec<InstructionCandidate> {
        Vec::new()
    }
    /// Project-local instruction/config files owned by this provider.
    fn project_instruction_candidates(&self, _project_path: &Path) -> Vec<InstructionCandidate> {
        Vec::new()
    }
    /// Project roots that should be inspected for project-scoped instruction artifacts.
    /// Session-capable providers get this for free from their known sessions. Instruction-only
    /// providers can override it without inventing fake session support.
    fn instruction_project_roots(&self) -> Vec<PathBuf> {
        let Ok(sessions) = self.list_sessions(None) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for session in sessions {
            let project_path = session.project_path.trim();
            if project_path.is_empty() || !seen.insert(project_path.to_string()) {
                continue;
            }
            roots.push(PathBuf::from(project_path));
        }
        roots
    }
    fn read_instruction_candidate(
        &self,
        candidate: &InstructionCandidate,
    ) -> Result<String, AppError> {
        if candidate.path.exists() {
            Ok(fs::read_to_string(&candidate.path)?)
        } else {
            Ok(String::new())
        }
    }
    fn save_instruction_candidate(
        &self,
        candidate: &InstructionCandidate,
        content: &str,
    ) -> Result<(), AppError> {
        if !candidate.editable {
            return Err(AppError::Archive(format!(
                "instruction artifact is read-only: {}",
                candidate.path.to_string_lossy()
            )));
        }
        if let Some(parent) = candidate.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&candidate.path, content)?;
        Ok(())
    }
    /// Native CLI command used to reopen a saved session. `extra_args` are user-configured raw
    /// flags for this provider, appended after the session selector.
    fn resume_command(&self, _session_id: &str, _extra_args: &[String]) -> Option<String> {
        None
    }
    fn list_rules(&self) -> Result<Vec<RuleFile>, AppError> {
        Ok(Vec::new())
    }
    fn get_rule(&self, category: &str, filename: &str) -> Result<RuleFile, AppError> {
        Err(AppError::NotFound(format!(
            "rule not found: {}/{}",
            category, filename
        )))
    }
    fn toggle_rule(&self, category: &str, filename: &str, _enabled: bool) -> Result<(), AppError> {
        Err(AppError::NotFound(format!(
            "rule not found: {}/{}",
            category, filename
        )))
    }
    fn list_memory_projects(&self) -> Result<Vec<ProjectInfo>, AppError> {
        Ok(Vec::new())
    }
    fn list_memories(&self, _project: &str) -> Result<Vec<MemoryFile>, AppError> {
        Ok(Vec::new())
    }
    fn get_memory(&self, project: &str, filename: &str) -> Result<MemoryFile, AppError> {
        Err(AppError::NotFound(format!(
            "memory not found: {}/{}",
            project, filename
        )))
    }
    fn save_memory(
        &self,
        project: &str,
        filename: &str,
        _frontmatter_data: &MemoryFrontmatter,
        _content: &str,
    ) -> Result<(), AppError> {
        Err(AppError::NotFound(format!(
            "memory not found: {}/{}",
            project, filename
        )))
    }
    fn delete_memory(&self, project: &str, filename: &str) -> Result<(), AppError> {
        Err(AppError::NotFound(format!(
            "memory not found: {}/{}",
            project, filename
        )))
    }
    fn list_profiles(&self) -> Result<Vec<ProfileArchive>, AppError> {
        Ok(Vec::new())
    }
    fn create_profile(&self, _name: Option<String>) -> Result<ProfileArchive, AppError> {
        Err(AppError::Archive(format!(
            "{} does not support snapshots",
            self.display_name()
        )))
    }
    fn restore_profile(&self, name: &str) -> Result<(), AppError> {
        Err(AppError::NotFound(format!("snapshot not found: {}", name)))
    }
    fn delete_profile(&self, name: &str) -> Result<(), AppError> {
        Err(AppError::NotFound(format!("snapshot not found: {}", name)))
    }
    fn rename_profile(&self, old_name: &str, _new_name: &str) -> Result<(), AppError> {
        Err(AppError::NotFound(format!(
            "snapshot not found: {}",
            old_name
        )))
    }
    fn first_prompt(
        &self,
        _project: &str,
        _session_id: &str,
        _archive: Option<&str>,
    ) -> Option<String> {
        None
    }
    fn session_detail(
        &self,
        _project: &str,
        _session_id: &str,
        _byte_offset: u64,
        _limit: u32,
        _min_level: &str,
        _archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        Err(AppError::Archive(format!(
            "{} does not support session detail",
            self.display_name()
        )))
    }
    fn session_tail(
        &self,
        _project: &str,
        _session_id: &str,
        _limit: u32,
        _min_level: &str,
        _archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        Err(AppError::Archive(format!(
            "{} does not support session tail",
            self.display_name()
        )))
    }
    /// Return the last `limit` display records strictly before `before_offset`, in chronological
    /// order. `start_byte_offset` in the result is the cursor for the next older page.
    fn session_before(
        &self,
        _project: &str,
        _session_id: &str,
        _before_offset: u64,
        _limit: u32,
        _min_level: &str,
        _archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        Err(AppError::Archive(format!(
            "{} does not support reverse session paging",
            self.display_name()
        )))
    }
    fn list_subagents(
        &self,
        _project: &str,
        _session_id: &str,
        _archive: Option<&str>,
    ) -> Result<Vec<SubagentInfo>, AppError> {
        Ok(Vec::new())
    }
    fn subagent_detail(
        &self,
        _project: &str,
        _session_id: &str,
        agent_id: &str,
        _byte_offset: u64,
        _limit: u32,
        _archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        Err(AppError::NotFound(format!(
            "{} has no subagent detail: {}",
            self.display_name(),
            agent_id
        )))
    }
    fn search_in_session(
        &self,
        _project: &str,
        _session_id: &str,
        _query: &str,
        _archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        Err(AppError::Archive(format!(
            "{} does not support session search",
            self.display_name()
        )))
    }
}

/// Holds the registered providers. Tauri-managed; commands resolve a provider by `source`.
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn AgentProvider>>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Arc<dyn AgentProvider>>) -> Self {
        Self { providers }
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    /// All registered providers (used to rebuild the global search index on demand).
    pub fn providers(&self) -> Vec<Arc<dyn AgentProvider>> {
        self.providers.clone()
    }

    /// Resolve the provider for a request: explicit `source`, else the first available provider.
    pub fn resolve(&self, source: Option<&str>) -> Option<Arc<dyn AgentProvider>> {
        match source {
            Some(s) => self.get(s),
            None => self
                .providers
                .iter()
                .find(|provider| provider.available())
                .cloned()
                .or_else(|| self.providers.first().cloned()),
        }
    }

    pub fn sources(&self) -> Vec<SourceInfo> {
        self.providers
            .iter()
            .map(|p| SourceInfo {
                id: p.id().to_string(),
                display_name: p.display_name().to_string(),
                available: p.available(),
                capabilities: p.capabilities(),
                hosts: p.hosts(),
            })
            .collect()
    }
}

pub use claude::ClaudeProvider;
pub use codex::CodexProvider;
pub use multi_host::MultiHostProvider;
pub use opencode::OpenCodeProvider;

/// One multi-host provider per agent. Each starts with only the native install registered; WSL
/// homes are adopted later (see `hosts::discover_wsl_homes`), because probing a distro's share
/// boots it and app launch must not wait on that.
pub fn default_providers(claude_paths: ClaudePaths) -> Vec<Arc<MultiHostProvider>> {
    vec![
        Arc::new(MultiHostProvider::new(
            Arc::new(ClaudeProvider::new(claude_paths)),
            Box::new(|host, home| {
                Arc::new(ClaudeProvider::for_host(host, ClaudePaths::for_home(home)))
            }),
        )),
        Arc::new(MultiHostProvider::new(
            Arc::new(CodexProvider::new()),
            Box::new(|host, home| Arc::new(CodexProvider::for_host(host, home))),
        )),
        Arc::new(MultiHostProvider::new(
            Arc::new(OpenCodeProvider::new()),
            Box::new(|host, home| Arc::new(OpenCodeProvider::for_host(host, home))),
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyProvider {
        id: &'static str,
        available: bool,
    }

    impl DummyProvider {
        fn new(id: &'static str, available: bool) -> Arc<Self> {
            Arc::new(Self { id, available })
        }
    }

    impl AgentProvider for DummyProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn display_name(&self) -> &'static str {
            self.id
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                sessions_read: true,
                sessions_search: true,
                sessions_resume: false,
                sessions_subagents: false,
                rules_read: false,
                rules_write: false,
                memory_read: false,
                memory_write: false,
                instructions_read: false,
                instructions_write: false,
                archive_read: false,
                archive_write: false,
                config_format: "json",
            }
        }

        fn available(&self) -> bool {
            self.available
        }
    }

    #[test]
    fn resolve_without_source_uses_first_available_provider() {
        let registry = ProviderRegistry::new(vec![
            DummyProvider::new("claude", false),
            DummyProvider::new("codex", true),
            DummyProvider::new("opencode", true),
        ]);

        let provider = registry.resolve(None).expect("provider");
        assert_eq!(provider.id(), "codex");
    }

    #[test]
    fn resolve_without_source_falls_back_to_first_registered_provider() {
        let registry = ProviderRegistry::new(vec![
            DummyProvider::new("claude", false),
            DummyProvider::new("codex", false),
        ]);

        let provider = registry.resolve(None).expect("provider");
        assert_eq!(provider.id(), "claude");
    }

    #[test]
    fn provider_can_omit_session_methods() {
        let provider = DummyProvider::new("instructions-only", true);

        assert!(provider.list_sessions(None).expect("sessions").is_empty());
        assert!(provider.first_prompt("project", "session", None).is_none());
        assert!(provider
            .list_subagents("project", "session", None)
            .expect("subagents")
            .is_empty());
        assert!(provider
            .session_detail("project", "session", 0, 100, "content", None)
            .is_err());
    }
}
