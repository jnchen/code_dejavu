use crate::agents::{bound_fast_index_texts, AgentProvider, IndexDoc, IndexText, TokenUsage};
use crate::models::session::{SessionModelInfo, SessionSummary};
use crate::paths::app_data_dir;
use jieba_rs::Jieba;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMeta {
    source: String,
    project_slug: String,
    project_path: String,
    session_id: String,
    file_size: u64,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    agent_title: Option<String>,
    modified: Option<String>,
    first_prompt: Option<String>,
    subagent_count: u32,
    archive_name: Option<String>,
    model_contexts: Vec<SessionModelInfo>,
    texts: Vec<IndexText>,
    tokens: TokenUsage,
    /// Provider doc identity (e.g. the session file path) — the incremental-cache key.
    key: String,
    /// Cheap content version ("size:mtime") used to detect changes without re-parsing.
    version: String,
    /// Deduped (kind, token) pairs (0=content, 1=tool, 2=reasoning). Arc-backed tokens are interned
    /// with the inverted-index keys, so runtime keeps one string allocation rather than one copy in
    /// every session plus another in the map. Serde still persists them as ordinary strings.
    index_tokens: Vec<(u8, Arc<str>)>,
}

/// Per-text-chunk byte budget kept only for snippet/substring display.
const SNIPPET_CAP_BYTES: usize = 16 * 1024;

/// Total preview bytes retained per session. The fast index is intentionally bounded; exhaustive
/// matches remain available through deep search, which scans the original source files.
const PREVIEW_BUDGET_BYTES: usize = 64 * 1024;

/// Hard unique-token limits per scope (content/tool/reasoning). Together with the provider-side
/// text cap, these bounds make both cold-build CPU and steady-state RAM predictable.
const INDEX_TOKEN_BUDGETS: [usize; 3] = [1024, 512, 512];
const MAX_INDEX_TOKENS_PER_SESSION: usize =
    INDEX_TOKEN_BUDGETS[0] + INDEX_TOKEN_BUDGETS[1] + INDEX_TOKEN_BUDGETS[2];
const MAX_INDEX_TOKEN_BYTES: usize = 128;

/// Index scope codes: 0 = content (对话/path), 1 = tool (工具 I/O), 2 = reasoning (思考).
fn kind_code(kind: &str) -> u8 {
    match kind {
        "tool" => 1,
        "reasoning" => 2,
        _ => 0,
    }
}

/// Keep only terms reachable through the non-literal fast-search path. Long blobs (base64,
/// delimiter-free hashes, minified payloads) caused the previous multi-gigabyte index and are
/// better served by literal/deep search.
fn normalized_index_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() || token.len() < 2 {
        return None;
    }
    let lowered = token.to_lowercase();
    if lowered.len() > MAX_INDEX_TOKEN_BYTES
        || !lowered.chars().any(|character| character.is_alphanumeric())
    {
        return None;
    }
    Some(lowered)
}

/// Selected search scopes → code set. Empty defaults to content only (avoids noisy collapsed hits).
fn scope_codes(scopes: &[String]) -> HashSet<u8> {
    let mut set = HashSet::new();
    for s in scopes {
        set.insert(kind_code(s));
    }
    if set.is_empty() {
        set.insert(0);
    }
    set
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveScope {
    Current,
    All,
    Archived,
}

impl ArchiveScope {
    fn from_value(value: Option<&str>) -> Self {
        match value {
            Some("current") => Self::Current,
            Some("all") => Self::All,
            Some("archived") => Self::Archived,
            _ => Self::Current,
        }
    }

    fn matches(self, archive_name: &Option<String>) -> bool {
        match self {
            Self::Current => archive_name.is_none(),
            Self::All => true,
            Self::Archived => archive_name.is_some(),
        }
    }
}

/// Queries with punctuation (IPs, paths, URLs, ids like `foo-bar`) are usually
/// literal lookups. Token-fuzzy search can split them into broad pieces such as
/// `47`/`95`, which produces surprising unrelated sessions.
fn is_literal_query(query: &str) -> bool {
    query
        .chars()
        .any(|c| !c.is_alphanumeric() && !c.is_whitespace())
}

#[derive(Debug, Clone)]
struct Hit {
    session_idx: u32,
    kind: u8,
}

#[derive(Debug, Clone, Serialize)]
pub enum IndexStatus {
    Building,
    Ready {
        session_count: usize,
        token_count: usize,
        /// Sessions/files that couldn't be read or parsed (surfaced so failures aren't silent).
        failed_files: usize,
    },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub sessions: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageBucket {
    pub key: String,
    pub sessions: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    pub totals: UsageTotals,
    pub by_source: Vec<UsageBucket>,
    pub by_model: Vec<UsageBucket>,
    pub by_project: Vec<UsageBucket>,
    pub by_day: Vec<UsageBucket>,
}

/// Per-source counts for the dashboard's "source health" row.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSourceStat {
    pub source: String,
    pub count: usize,
    pub last_active: Option<String>,
}

/// One day in the dashboard activity histogram.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardActivityDay {
    pub day: String,
    pub count: usize,
}

/// A busiest-project entry for the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardProject {
    pub path: String,
    pub count: usize,
    pub last_active: String,
}

/// Everything the dashboard needs, pre-aggregated from the in-memory index so the homepage
/// never re-scans session files on disk. Current sessions only (archived snapshots excluded).
#[derive(Debug, Serialize)]
pub struct DashboardSummary {
    pub total_sessions: usize,
    pub recent: Vec<SessionSummary>,
    pub by_source: Vec<DashboardSourceStat>,
    pub activity: Vec<DashboardActivityDay>,
    pub top_projects: Vec<DashboardProject>,
}

pub struct SearchEngine {
    sessions: Arc<Vec<SessionMeta>>,
    inverted: HashMap<Arc<str>, Vec<Hit>>,
    jieba: Arc<Jieba>,
    pub status: IndexStatus,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub session: SessionSummary,
    pub snippet: String,
}

impl SearchEngine {
    /// Assemble an engine from already-processed sessions: rebuilds the inverted index from each
    /// session's cached tokens (cheap, in-memory, no jieba, no disk reads).
    fn from_sessions(
        jieba: Arc<Jieba>,
        mut sessions: Vec<SessionMeta>,
        failed_files: usize,
    ) -> Self {
        let inverted = build_inverted(&mut sessions);
        let session_count = sessions.len();
        let token_count = inverted.len();
        eprintln!(
            "[dejavu] 索引完成: {} 个会话, {} 个 token, {} 个文件解析失败",
            session_count, token_count, failed_files
        );
        Self {
            sessions: Arc::new(sessions),
            inverted,
            jieba,
            status: IndexStatus::Ready {
                session_count,
                token_count,
                failed_files,
            },
        }
    }

    /// Clone only the Arc around the immutable session list while holding the engine lock. The
    /// heavier incremental-map construction then happens after the lock is released, so searches
    /// and page loads never wait behind it.
    fn sessions_snapshot(&self) -> Arc<Vec<SessionMeta>> {
        self.sessions.clone()
    }

    /// Search within `scopes` (content/tool/reasoning), optionally filtered to one `source`.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        scopes: &[String],
        source: Option<&str>,
        archive_scope: Option<&str>,
    ) -> Vec<SearchResult> {
        let scope_set = scope_codes(scopes);
        let archive_scope = ArchiveScope::from_value(archive_scope);
        let query_lower = query.trim().to_lowercase();
        if query_lower.is_empty() {
            return self.browse(limit, source, archive_scope);
        }

        if is_literal_query(&query_lower) {
            return self.substring_search(&query_lower, limit, &scope_set, source, archive_scope);
        }

        let tokens: Vec<String> = self
            .jieba
            .cut(&query_lower, false)
            .into_iter()
            .filter_map(normalized_index_token)
            .collect();

        if tokens.is_empty() {
            return self.substring_search(&query_lower, limit, &scope_set, source, archive_scope);
        }

        let in_scope = |idx: u32| {
            let session = &self.sessions[idx as usize];
            source.is_none_or(|s| session.source == s)
                && archive_scope.matches(&session.archive_name)
        };

        // Score = number of distinct query tokens matched in an in-scope text of the session.
        let mut session_tokens: HashMap<u32, HashSet<&str>> = HashMap::new();
        for token in &tokens {
            if let Some(hits) = self.inverted.get(token.as_str()) {
                for hit in hits {
                    if scope_set.contains(&hit.kind) && in_scope(hit.session_idx) {
                        session_tokens
                            .entry(hit.session_idx)
                            .or_default()
                            .insert(token.as_str());
                    }
                }
            }
        }

        // Recall over precision: keep every session that matched ANY query token. The old
        // `>= 50% of tokens` filter silently hid partial matches ("I know that word is in there
        // but it won't show up"). Rank by number of matched tokens, then most-recent-first, so
        // results are complete, stable, and surface recent work at the top.
        let mut scored: Vec<(u32, u32)> = session_tokens
            .into_iter()
            .map(|(idx, toks)| (idx, toks.len() as u32))
            .collect();
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| {
                self.sessions[b.0 as usize]
                    .modified
                    .cmp(&self.sessions[a.0 as usize].modified)
            })
        });

        let mut results: Vec<SearchResult> = Vec::new();
        let mut seen: HashSet<(String, String, Option<String>)> = HashSet::new();
        for (idx, _) in scored.into_iter().take(limit) {
            let session = &self.sessions[idx as usize];
            seen.insert((
                session.source.clone(),
                session.session_id.clone(),
                session.archive_name.clone(),
            ));
            results.push(SearchResult {
                session: to_summary(session),
                snippet: self.find_snippet(session, &query_lower, &scope_set),
            });
        }

        // Fuse in substring matches jieba's tokenizer split away — e.g. a fragment embedded inside
        // a longer identifier, or text that isn't on a token boundary. Token hits rank first;
        // substring-only hits fill the rest up to `limit`, so nothing the user can see is hidden.
        if results.len() < limit {
            for r in self.substring_search(&query_lower, limit, &scope_set, source, archive_scope) {
                if results.len() >= limit {
                    break;
                }
                let key = (
                    r.session.source.clone(),
                    r.session.session_id.clone(),
                    r.session.archive_name.clone(),
                );
                if seen.insert(key) {
                    results.push(r);
                }
            }
        }
        results
    }

    /// Aggregate token usage across all indexed sessions, bucketed by source / model / project /
    /// day. Current sessions only (archived snapshots excluded) so totals reflect live work.
    pub fn usage_summary(&self) -> UsageSummary {
        let mut totals = UsageTotals::default();
        let mut by_source: HashMap<String, (usize, TokenUsage)> = HashMap::new();
        let mut by_model: HashMap<String, (usize, TokenUsage)> = HashMap::new();
        let mut by_project: HashMap<String, (usize, TokenUsage)> = HashMap::new();
        let mut by_day: HashMap<String, (usize, TokenUsage)> = HashMap::new();

        let bump =
            |map: &mut HashMap<String, (usize, TokenUsage)>, key: String, tk: &TokenUsage| {
                let e = map.entry(key).or_default();
                e.0 += 1;
                e.1.input_tokens += tk.input_tokens;
                e.1.output_tokens += tk.output_tokens;
                e.1.cache_tokens += tk.cache_tokens;
                e.1.total_tokens += tk.total_tokens;
            };

        for s in self.sessions.iter() {
            if s.archive_name.is_some() {
                continue;
            }
            totals.sessions += 1;
            totals.input_tokens += s.tokens.input_tokens;
            totals.output_tokens += s.tokens.output_tokens;
            totals.cache_tokens += s.tokens.cache_tokens;
            totals.total_tokens += s.tokens.total_tokens;

            bump(&mut by_source, s.source.clone(), &s.tokens);
            let model = s
                .model_contexts
                .iter()
                .find_map(|m| m.model.clone())
                .unwrap_or_else(|| "未知".to_string());
            bump(&mut by_model, model, &s.tokens);
            bump(&mut by_project, s.project_path.clone(), &s.tokens);
            if let Some(day) = s
                .modified
                .as_deref()
                .map(|m| m.chars().take(10).collect::<String>())
            {
                if !day.is_empty() {
                    bump(&mut by_day, day, &s.tokens);
                }
            }
        }

        let by_tokens = |map: HashMap<String, (usize, TokenUsage)>| -> Vec<UsageBucket> {
            let mut v: Vec<UsageBucket> = map.into_iter().map(to_bucket).collect();
            v.sort_by(|a, b| {
                b.total_tokens
                    .cmp(&a.total_tokens)
                    .then_with(|| b.sessions.cmp(&a.sessions))
                    .then_with(|| a.key.cmp(&b.key))
            });
            v
        };
        let by_key = |map: HashMap<String, (usize, TokenUsage)>| -> Vec<UsageBucket> {
            let mut v: Vec<UsageBucket> = map.into_iter().map(to_bucket).collect();
            v.sort_by(|a, b| a.key.cmp(&b.key));
            v
        };

        let mut by_project = by_tokens(by_project);
        by_project.truncate(12);
        UsageSummary {
            totals,
            by_source: by_tokens(by_source),
            by_model: by_tokens(by_model),
            by_project,
            by_day: by_key(by_day),
        }
    }

    /// Every concrete model id present in current session metadata. Unlike the usage buckets,
    /// this includes all model contexts from sessions that switched models mid-conversation.
    pub fn discovered_models(&self) -> Vec<String> {
        let mut models: HashSet<String> = self
            .sessions
            .iter()
            .filter(|session| session.archive_name.is_none())
            .flat_map(|session| session.model_contexts.iter())
            .filter_map(|context| context.model.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty() && *model != "未知")
            .map(str::to_string)
            .collect();
        let mut models: Vec<String> = models.drain().collect();
        models.sort_by_key(|model| model.to_ascii_lowercase());
        models
    }

    /// Pre-aggregate the dashboard view (recent / activity / top projects / per-source counts)
    /// straight from the in-memory index — no disk I/O. Current sessions only (archived excluded),
    /// matching the homepage's previous `list_sessions`-based behavior but ~free once indexed.
    pub fn dashboard_summary(&self) -> DashboardSummary {
        const RECENT: usize = 10;
        const TOP_PROJECTS: usize = 8;
        const ACTIVITY_DAYS: i64 = 30;

        let current: Vec<&SessionMeta> = self
            .sessions
            .iter()
            .filter(|s| s.archive_name.is_none())
            .collect();

        // Most-recent sessions first.
        let mut recent_refs = current.clone();
        recent_refs.sort_by(|a, b| {
            b.modified
                .as_deref()
                .unwrap_or("")
                .cmp(a.modified.as_deref().unwrap_or(""))
        });
        let recent: Vec<SessionSummary> = recent_refs
            .into_iter()
            .take(RECENT)
            .map(to_summary)
            .collect();

        // Per-source count + last-active.
        let mut src_map: HashMap<String, (usize, String)> = HashMap::new();
        for s in &current {
            let entry = src_map.entry(s.source.clone()).or_default();
            entry.0 += 1;
            if let Some(m) = s.modified.as_deref() {
                if m > entry.1.as_str() {
                    entry.1 = m.to_string();
                }
            }
        }
        let mut by_source: Vec<DashboardSourceStat> = src_map
            .into_iter()
            .map(|(source, (count, last))| DashboardSourceStat {
                source,
                count,
                last_active: if last.is_empty() { None } else { Some(last) },
            })
            .collect();
        by_source.sort_by(|a, b| a.source.cmp(&b.source));

        // Busiest projects.
        let mut proj_map: HashMap<String, (usize, String)> = HashMap::new();
        for s in &current {
            let key = if s.project_path.is_empty() {
                s.project_slug.clone()
            } else {
                s.project_path.clone()
            };
            let entry = proj_map.entry(key).or_default();
            entry.0 += 1;
            if let Some(m) = s.modified.as_deref() {
                if m > entry.1.as_str() {
                    entry.1 = m.to_string();
                }
            }
        }
        let mut top_projects: Vec<DashboardProject> = proj_map
            .into_iter()
            .map(|(path, (count, last_active))| DashboardProject {
                path,
                count,
                last_active,
            })
            .collect();
        top_projects.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_active.cmp(&a.last_active))
        });
        top_projects.truncate(TOP_PROJECTS);

        // Sessions-per-day over a trailing window. `modified` is local "YYYY-MM-DD HH:MM", so a
        // local date key lines the buckets up with what the user sees.
        let mut day_counts: HashMap<String, usize> = HashMap::new();
        for s in &current {
            if let Some(m) = s.modified.as_deref() {
                let day: String = m.chars().take(10).collect();
                if !day.is_empty() {
                    *day_counts.entry(day).or_insert(0) += 1;
                }
            }
        }
        let today = chrono::Local::now().date_naive();
        let mut activity = Vec::with_capacity(ACTIVITY_DAYS as usize);
        for i in (0..ACTIVITY_DAYS).rev() {
            let day = (today - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            let count = day_counts.get(&day).copied().unwrap_or(0);
            activity.push(DashboardActivityDay { day, count });
        }

        DashboardSummary {
            total_sessions: current.len(),
            recent,
            by_source,
            activity,
            top_projects,
        }
    }

    /// Session summaries within a source + archive scope, used as candidates for deep full-text
    /// search (which then scans each one's source file via the provider).
    pub fn sessions_in_scope(
        &self,
        source: Option<&str>,
        archive_scope: Option<&str>,
    ) -> Vec<SessionSummary> {
        let scope = ArchiveScope::from_value(archive_scope);
        self.sessions
            .iter()
            .filter(|s| source.is_none_or(|src| s.source == src) && scope.matches(&s.archive_name))
            .map(to_summary)
            .collect()
    }

    /// Recency-sorted session summaries for the browser's plain (no-query) list, served straight
    /// from the in-memory index — no per-file disk scan and no per-session first-prompt reads
    /// (summaries already carry `first_prompt`). Unbounded on purpose so it fully replaces the
    /// provider's `list_sessions` for the default view without hiding sessions behind a cap.
    pub fn browse_summaries(
        &self,
        source: Option<&str>,
        archive_scope: Option<&str>,
    ) -> Vec<SessionSummary> {
        let scope = ArchiveScope::from_value(archive_scope);
        let mut sessions: Vec<&SessionMeta> = self
            .sessions
            .iter()
            .filter(|s| source.is_none_or(|src| s.source == src) && scope.matches(&s.archive_name))
            .collect();
        sessions.sort_by(|a, b| {
            b.modified
                .as_deref()
                .unwrap_or("")
                .cmp(a.modified.as_deref().unwrap_or(""))
        });
        sessions.into_iter().map(to_summary).collect()
    }

    fn browse(
        &self,
        limit: usize,
        source: Option<&str>,
        archive_scope: ArchiveScope,
    ) -> Vec<SearchResult> {
        let mut sessions: Vec<&SessionMeta> = self
            .sessions
            .iter()
            .filter(|session| {
                source.is_none_or(|s| session.source == s)
                    && archive_scope.matches(&session.archive_name)
            })
            .collect();
        sessions.sort_by(|a, b| {
            b.modified
                .as_deref()
                .unwrap_or("")
                .cmp(a.modified.as_deref().unwrap_or(""))
        });
        sessions
            .into_iter()
            .take(limit)
            .map(|session| SearchResult {
                session: to_summary(session),
                snippet: session
                    .first_prompt
                    .clone()
                    .unwrap_or_else(|| session.project_path.clone()),
            })
            .collect()
    }

    fn substring_search(
        &self,
        query: &str,
        limit: usize,
        scope_set: &HashSet<u8>,
        source: Option<&str>,
        archive_scope: ArchiveScope,
    ) -> Vec<SearchResult> {
        self.sessions
            .par_iter()
            .filter_map(|session| {
                if let Some(s) = source {
                    if session.source != s {
                        return None;
                    }
                }
                if !archive_scope.matches(&session.archive_name) {
                    return None;
                }
                if scope_set.contains(&0) && session.project_path.to_lowercase().contains(query) {
                    return Some(SearchResult {
                        session: to_summary(session),
                        snippet: session.project_path.clone(),
                    });
                }
                if scope_set.contains(&0) && session.session_id.to_lowercase().contains(query) {
                    return Some(SearchResult {
                        session: to_summary(session),
                        snippet: session.session_id.clone(),
                    });
                }
                if scope_set.contains(&0)
                    && session
                        .agent_title
                        .as_deref()
                        .is_some_and(|title| title.to_lowercase().contains(query))
                {
                    return Some(SearchResult {
                        session: to_summary(session),
                        snippet: session.agent_title.clone().unwrap_or_default(),
                    });
                }
                if scope_set.contains(&0)
                    && session
                        .first_prompt
                        .as_deref()
                        .is_some_and(|prompt| prompt.to_lowercase().contains(query))
                {
                    return Some(SearchResult {
                        session: to_summary(session),
                        snippet: session.first_prompt.clone().unwrap_or_default(),
                    });
                }
                for it in &session.texts {
                    if scope_set.contains(&kind_code(&it.kind))
                        && it.text.to_lowercase().contains(query)
                    {
                        return Some(SearchResult {
                            session: to_summary(session),
                            snippet: extract_snippet(&it.text, query),
                        });
                    }
                }
                None
            })
            .collect::<Vec<_>>()
            .into_iter()
            .take(limit)
            .collect()
    }

    fn find_snippet(&self, session: &SessionMeta, query: &str, scope_set: &HashSet<u8>) -> String {
        for it in &session.texts {
            if scope_set.contains(&kind_code(&it.kind)) && it.text.to_lowercase().contains(query) {
                return extract_snippet(&it.text, query);
            }
        }
        if session.project_path.to_lowercase().contains(query) {
            return session.project_path.clone();
        }
        if let Some(title) = session
            .agent_title
            .as_ref()
            .filter(|title| title.to_lowercase().contains(query))
        {
            return title.clone();
        }
        session
            .first_prompt
            .clone()
            .unwrap_or_else(|| session.project_path.clone())
    }
}

pub fn extract_snippet(text: &str, query: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower_chars: Vec<char> = text.to_lowercase().chars().collect();
    let query_chars: Vec<char> = query.chars().collect();

    let pos = lower_chars
        .windows(query_chars.len().max(1))
        .position(|w| w == query_chars.as_slice())
        .unwrap_or(0);

    let start = pos.saturating_sub(40);
    let end = (pos + query_chars.len() + 100).min(chars.len());
    let snippet: String = chars[start..end].iter().collect();
    let prefix = if start > 0 { "..." } else { "" };
    let suffix = if end < chars.len() { "..." } else { "" };
    format!("{}{}{}", prefix, snippet, suffix)
}

impl From<IndexDoc> for SessionMeta {
    fn from(doc: IndexDoc) -> Self {
        Self {
            source: doc.source,
            project_slug: doc.project,
            project_path: doc.project_path,
            session_id: doc.session_id,
            file_size: doc.file_size_bytes,
            created_at: doc.created_at,
            updated_at: doc.updated_at.clone(),
            agent_title: doc.agent_title,
            modified: doc.updated_at.or(doc.timestamp),
            first_prompt: doc.first_prompt,
            subagent_count: doc.subagent_count,
            archive_name: doc.archive_name,
            model_contexts: doc.model_contexts,
            texts: doc.texts,
            tokens: doc.tokens,
            key: doc.key,
            version: doc.version,
            index_tokens: Vec::new(),
        }
    }
}

fn add_index_tokens(jieba: &Jieba, code: u8, text: &str, seen: &mut [HashSet<String>; 3]) {
    let scope = usize::from(code.min(2));
    if seen[scope].len() >= INDEX_TOKEN_BUDGETS[scope] {
        return;
    }
    for token in jieba.cut(text, false) {
        let Some(token) = normalized_index_token(token) else {
            continue;
        };
        seen[scope].insert(token);
        if seen[scope].len() >= INDEX_TOKEN_BUDGETS[scope] {
            break;
        }
    }
}

fn truncate_utf8_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Tokenize one parsed doc into a strictly bounded fast index and retain a small display preview.
/// Exhaustive recall is intentionally delegated to deep search, which scans the original source.
fn process_doc(jieba: &Jieba, mut doc: IndexDoc) -> SessionMeta {
    // Enforce the provider boundary defensively for direct/future IndexDoc producers too.
    doc.texts = bound_fast_index_texts(std::mem::take(&mut doc.texts));

    let mut seen = [HashSet::new(), HashSet::new(), HashSet::new()];
    // Metadata is small and useful, so reserve its place before conversational chunks consume the
    // content-scope budget.
    add_index_tokens(jieba, 0, &doc.project_path, &mut seen);
    add_index_tokens(jieba, 0, &doc.session_id, &mut seen);
    if let Some(first_prompt) = doc.first_prompt.as_deref() {
        add_index_tokens(jieba, 0, first_prompt, &mut seen);
    }
    if let Some(agent_title) = doc.agent_title.as_deref() {
        add_index_tokens(jieba, 0, agent_title, &mut seen);
    }
    for it in &doc.texts {
        let code = kind_code(&it.kind);
        add_index_tokens(jieba, code, &it.text, &mut seen);
    }

    let mut index_tokens = Vec::with_capacity(MAX_INDEX_TOKENS_PER_SESSION);
    for (code, tokens) in seen.into_iter().enumerate() {
        for token in tokens {
            index_tokens.push((code as u8, Arc::<str>::from(token)));
        }
    }
    index_tokens.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    // Keep a byte-bounded preview copy for snippets and fast substring matches.
    let mut preview: Vec<IndexText> = Vec::new();
    let mut remaining = PREVIEW_BUDGET_BYTES;
    for it in doc.texts.drain(..) {
        if remaining == 0 {
            break;
        }
        let mut text = it.text;
        truncate_utf8_bytes(&mut text, SNIPPET_CAP_BYTES.min(remaining));
        if text.is_empty() {
            continue;
        }
        remaining -= text.len();
        preview.push(IndexText {
            kind: it.kind,
            text,
        });
    }
    doc.texts = preview;
    let mut meta = SessionMeta::from(doc);
    meta.index_tokens = index_tokens;
    meta
}

/// Rebuild the inverted index from each session's cached tokens. Position in `sessions` is the
/// `session_idx` used by `Hit`, so this must run whenever the session list changes. Duplicate Arc
/// allocations from cache/provider parsing are replaced with the canonical map key as we build.
fn build_inverted(sessions: &mut [SessionMeta]) -> HashMap<Arc<str>, Vec<Hit>> {
    let mut inverted: HashMap<Arc<str>, Vec<Hit>> = HashMap::new();
    for (idx, session) in sessions.iter_mut().enumerate() {
        let sidx = idx as u32;
        for (code, token) in &mut session.index_tokens {
            let hit = Hit {
                session_idx: sidx,
                kind: *code,
            };
            match inverted.entry(token.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let canonical = entry.key().clone();
                    entry.get_mut().push(hit);
                    *token = canonical;
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(vec![hit]);
                }
            }
        }
    }
    inverted
}

/// Parse and process every session from every searchable provider.
fn build_sessions_full(
    jieba: &Jieba,
    providers: &[Arc<dyn AgentProvider>],
) -> (Vec<SessionMeta>, usize) {
    let mut sessions = Vec::new();
    let mut failed = 0;
    for p in providers {
        if !p.capabilities().sessions_search {
            continue;
        }
        let batch = p.index_documents();
        failed += batch.failed;
        for doc in batch.docs {
            sessions.push(process_doc(jieba, doc));
        }
    }
    (sessions, failed)
}

fn session_map_from(sessions: &[SessionMeta]) -> HashMap<(String, String), SessionMeta> {
    sessions
        .iter()
        .map(|session| {
            (
                (session.source.clone(), session.key.clone()),
                session.clone(),
            )
        })
        .collect()
}

/// Incremental build: reuse `prev` sessions whose content version is unchanged, re-parse only the
/// changed/new ones. Providers without a manifest (e.g. the DB-backed OpenCode) fall back to a full
/// reparse, which is still correct.
fn build_sessions_incremental(
    jieba: &Jieba,
    providers: &[Arc<dyn AgentProvider>],
    prev: &HashMap<(String, String), SessionMeta>,
) -> (Vec<SessionMeta>, usize) {
    let mut sessions = Vec::new();
    let mut failed = 0;
    for p in providers {
        if !p.capabilities().sessions_search {
            continue;
        }
        let source = p.id().to_string();
        let manifest = p.index_manifest();
        if manifest.is_empty() {
            // No incremental support — full reparse, replacing all this provider's docs.
            let batch = p.index_documents();
            failed += batch.failed;
            for doc in batch.docs {
                sessions.push(process_doc(jieba, doc));
            }
            continue;
        }
        // Which manifest keys changed (or are new) vs the previous cache?
        let mut changed: HashSet<String> = HashSet::new();
        for entry in &manifest {
            let unchanged = prev
                .get(&(source.clone(), entry.key.clone()))
                .map(|m| m.version == entry.version)
                .unwrap_or(false);
            if !unchanged {
                changed.insert(entry.key.clone());
            }
        }
        let mut parsed: HashMap<String, SessionMeta> = HashMap::new();
        if !changed.is_empty() {
            let batch = p.index_documents_for(&changed);
            failed += batch.failed;
            for doc in batch.docs {
                let meta = process_doc(jieba, doc);
                parsed.insert(meta.key.clone(), meta);
            }
        }
        // Emit in manifest order: freshly parsed if available, else the unchanged cached copy.
        // A changed key that yielded no doc is a content-less session (silent skip), not a failure.
        for entry in manifest {
            if let Some(meta) = parsed.remove(&entry.key) {
                sessions.push(meta);
            } else if let Some(meta) = prev.get(&(source.clone(), entry.key.clone())) {
                sessions.push(meta.clone());
            }
        }
    }
    (sessions, failed)
}

// v3 is bounded and stores Arc-backed fast-index tokens. The old JSON cache can be hundreds of
// megabytes and must never be read into memory again; this version is streamed and capped.
const CACHE_SCHEMA: u32 = 4;
const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHED_SESSIONS: usize = 100_000;

#[derive(Serialize, Deserialize)]
struct IndexCache {
    schema: u32,
    sessions: Vec<SessionMeta>,
}

fn cache_path() -> PathBuf {
    app_data_dir().join("search-index-cache-v3.json")
}

fn cache_file_size_allowed(size: u64) -> bool {
    size <= MAX_CACHE_BYTES
}

fn cached_sessions_within_limits(sessions: &[SessionMeta]) -> bool {
    if sessions.len() > MAX_CACHED_SESSIONS {
        return false;
    }
    sessions.iter().all(|session| {
        if session.index_tokens.len() > MAX_INDEX_TOKENS_PER_SESSION {
            return false;
        }
        let mut scope_counts = [0usize; 3];
        for (code, token) in &session.index_tokens {
            let scope = usize::from(*code);
            if scope >= scope_counts.len()
                || token.len() < 2
                || token.len() > MAX_INDEX_TOKEN_BYTES
                || !token.chars().any(|character| character.is_alphanumeric())
            {
                return false;
            }
            scope_counts[scope] += 1;
            if scope_counts[scope] > INDEX_TOKEN_BUDGETS[scope] {
                return false;
            }
        }
        let preview_bytes = session
            .texts
            .iter()
            .try_fold(0usize, |total, text| total.checked_add(text.text.len()));
        preview_bytes.is_some_and(|bytes| bytes <= PREVIEW_BUDGET_BYTES)
            && session
                .texts
                .iter()
                .all(|text| text.text.len() <= SNIPPET_CAP_BYTES)
    })
}

#[cfg(test)]
fn validated_cache_sessions(cache: IndexCache) -> Option<Vec<SessionMeta>> {
    if cache.schema != CACHE_SCHEMA || !cached_sessions_within_limits(&cache.sessions) {
        return None;
    }
    Some(cache.sessions)
}

fn remove_legacy_cache() {
    for path in [
        app_data_dir().join("search-index-cache.json"),
        app_data_dir().join("search-index-cache.json.tmp"),
        app_data_dir().join("search-index-cache-v3.bin"),
        app_data_dir().join("search-index-cache-v3.bin.tmp"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => eprintln!("[dejavu] 已移除旧版无界索引缓存: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "[dejavu] 无法移除旧版索引缓存 {}: {}",
                path.display(),
                error
            ),
        }
    }
}

/// Load the persisted index so search/usage/dashboard work instantly on launch (before the disk
/// reconcile finishes). Returns None if absent, unreadable, or written by an older schema.
fn load_cached_sessions() -> Option<Vec<SessionMeta>> {
    let path = cache_path();
    let size = match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[dejavu] 无法读取索引缓存元数据: {error}");
            }
            return None;
        }
    };
    if !cache_file_size_allowed(size) {
        eprintln!(
            "[dejavu] 拒绝加载过大索引缓存: {} bytes ({})",
            size,
            path.display()
        );
        return None;
    }
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("[dejavu] 无法打开索引缓存: {error}");
            return None;
        }
    };
    let cache: IndexCache = match serde_json::from_reader(BufReader::new(file)) {
        Ok(cache) => cache,
        Err(error) => {
            eprintln!("[dejavu] 无法解析索引缓存: {error}");
            return None;
        }
    };
    if cache.schema != CACHE_SCHEMA {
        eprintln!(
            "[dejavu] 忽略旧版索引缓存 schema={}，当前 schema={}",
            cache.schema, CACHE_SCHEMA
        );
        return None;
    }
    if !cached_sessions_within_limits(&cache.sessions) {
        eprintln!("[dejavu] 索引缓存内容超出安全边界，已忽略");
        return None;
    }
    Some(cache.sessions)
}

fn replace_cache_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    let backup = path.with_extension("json.bak");
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    let had_previous = path.exists();
    if had_previous {
        std::fs::rename(path, &backup)?;
    }
    match std::fs::rename(tmp, path) {
        Ok(()) => {
            if had_previous {
                let _ = std::fs::remove_file(backup);
            }
            Ok(())
        }
        Err(error) => {
            if had_previous {
                let _ = std::fs::rename(&backup, path);
            }
            Err(error)
        }
    }
}

/// Persist the processed index (metadata + capped texts + tokens) for the next cold start. Written
/// atomically (temp + rename) so a crash mid-write can't corrupt the cache.
fn save_cached_sessions(sessions: &[SessionMeta]) {
    #[derive(Serialize)]
    struct IndexCacheRef<'a> {
        schema: u32,
        sessions: &'a [SessionMeta],
    }
    if !cached_sessions_within_limits(sessions) {
        eprintln!("[dejavu] 跳过超出边界的索引缓存写入");
        return;
    }
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = IndexCacheRef {
        schema: CACHE_SCHEMA,
        sessions,
    };
    let tmp = path.with_extension("json.tmp");
    let Ok(file) = File::create(&tmp) else {
        return;
    };
    let mut writer = BufWriter::new(file);
    if serde_json::to_writer(&mut writer, &payload).is_err() || writer.flush().is_err() {
        drop(writer);
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    drop(writer);
    if replace_cache_file(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // Reset the throttle window so a forced write (cold start / explicit rebuild) doesn't get
    // immediately followed by a throttled one.
    if let Ok(mut guard) = persist_clock().lock() {
        *guard = Some(Instant::now());
    }
}

/// Minimum gap between on-disk cache rewrites from the high-frequency auto-refresh path.
const MIN_PERSIST_INTERVAL: Duration = Duration::from_secs(60);

fn persist_clock() -> &'static Mutex<Option<Instant>> {
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// Like [`save_cached_sessions`] but skips the write when the previous persist was under
/// [`MIN_PERSIST_INTERVAL`] ago. The in-memory index is always current; only the (potentially
/// large) cache file write is coalesced. Any skipped state is reconciled from disk on the next
/// cold start, so a slightly-stale cache is harmless.
fn save_cached_sessions_throttled(sessions: &[SessionMeta]) {
    {
        let Ok(guard) = persist_clock().lock() else {
            return;
        };
        if let Some(prev) = *guard {
            if prev.elapsed() < MIN_PERSIST_INTERVAL {
                return;
            }
        }
    }
    save_cached_sessions(sessions);
}

fn to_bucket((key, (sessions, tk)): (String, (usize, TokenUsage))) -> UsageBucket {
    UsageBucket {
        key,
        sessions,
        input_tokens: tk.input_tokens,
        output_tokens: tk.output_tokens,
        cache_tokens: tk.cache_tokens,
        total_tokens: tk.total_tokens,
    }
}

fn to_summary(meta: &SessionMeta) -> SessionSummary {
    SessionSummary {
        source: meta.source.clone(),
        session_id: meta.session_id.clone(),
        project: meta.project_slug.clone(),
        project_path: meta.project_path.clone(),
        first_prompt: meta.first_prompt.clone(),
        agent_title: meta.agent_title.clone(),
        created_at: meta.created_at.clone(),
        updated_at: meta.updated_at.clone().or_else(|| meta.modified.clone()),
        timestamp: meta.modified.clone(),
        file_size_bytes: meta.file_size,
        subagent_count: meta.subagent_count,
        archive_name: meta.archive_name.clone(),
        model_contexts: meta.model_contexts.clone(),
    }
}

pub type SharedSearchEngine = Arc<RwLock<SearchEngine>>;
static INDEX_WORK_ACTIVE: AtomicBool = AtomicBool::new(false);
static INDEX_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// Search reconciliation is throughput work. Keep it on its own small pool so a cold index build
/// cannot occupy the Rayon workers used by latency-sensitive project/rule/instruction commands.
fn index_pool() -> &'static rayon::ThreadPool {
    INDEX_POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(|count| count.get().saturating_sub(1).clamp(1, 2))
            .unwrap_or(1);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("dejavu-index-{index}"))
            .build()
            .expect("build search index thread pool")
    })
}

fn index_work_active() -> bool {
    INDEX_WORK_ACTIVE.load(Ordering::Acquire)
}

struct IndexWorkGuard;

impl Drop for IndexWorkGuard {
    fn drop(&mut self) {
        INDEX_WORK_ACTIVE.store(false, Ordering::Release);
    }
}

fn try_begin_index_work() -> Option<IndexWorkGuard> {
    INDEX_WORK_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| IndexWorkGuard)
}

pub fn build_in_background(providers: Vec<Arc<dyn AgentProvider>>) -> SharedSearchEngine {
    let jieba = Arc::new(Jieba::new());
    let engine = Arc::new(RwLock::new(SearchEngine {
        sessions: Arc::new(Vec::new()),
        inverted: HashMap::new(),
        jieba: jieba.clone(),
        status: IndexStatus::Building,
    }));

    INDEX_WORK_ACTIVE.store(true, Ordering::Release);
    let work_guard = IndexWorkGuard;
    let engine_clone = engine.clone();
    std::thread::spawn(move || {
        let _work_guard = work_guard;
        index_pool().install(|| {
            remove_legacy_cache();
            // 1. Instant availability: if a persisted index exists, load it so search / usage /
            //    dashboard work immediately on launch instead of waiting for a full disk scan.
            if let Some(sessions) = load_cached_sessions() {
                let built = SearchEngine::from_sessions(jieba.clone(), sessions, 0);
                if let Ok(mut guard) = engine_clone.write() {
                    *guard = built;
                }
            }
            // 2. Reconcile against disk incrementally: with no cache this parses everything (cold
            //    start); with a cache it only re-parses sessions that changed since last run.
            let snapshot = engine_clone
                .read()
                .map(|guard| guard.sessions_snapshot())
                .unwrap_or_else(|_| Arc::new(Vec::new()));
            let prev = session_map_from(&snapshot);
            let (sessions, failed) = build_sessions_incremental(&jieba, &providers, &prev);
            save_cached_sessions(&sessions);
            let built = SearchEngine::from_sessions(jieba.clone(), sessions, failed);
            if let Ok(mut guard) = engine_clone.write() {
                *guard = built;
            }
        });
    });

    engine
}

/// Full, from-scratch rebuild in place (the explicit "rebuild index" action). Ignores the cache to
/// recover from any drift, then re-persists the fresh index.
pub fn rebuild(engine: &SharedSearchEngine, providers: &[Arc<dyn AgentProvider>]) -> bool {
    let Some(_work_guard) = try_begin_index_work() else {
        return false;
    };
    if let Ok(mut guard) = engine.write() {
        guard.status = IndexStatus::Building;
    }
    index_pool().install(|| {
        let jieba = Arc::new(Jieba::new());
        let (sessions, failed) = build_sessions_full(&jieba, providers);
        save_cached_sessions(&sessions);
        let built = SearchEngine::from_sessions(jieba, sessions, failed);
        if let Ok(mut guard) = engine.write() {
            *guard = built;
        }
    });
    true
}

/// Cheap change fingerprint over every provider's watched roots.
///
/// The path fingerprint matters for snapshot operations: moving an OpenCode database from the
/// live data directory into an archive preserves its size and mtime, so a count/latest-mtime pair
/// alone would miss the move and leave the index pointing at the deleted live database.
/// Only metadata is read (never file contents), so this stays fast even for large session stores.
fn roots_fingerprint(providers: &[Arc<dyn AgentProvider>]) -> (u64, u64, u64) {
    let mut count = 0u64;
    let mut latest = 0u64;
    let mut paths = 0u64;
    for provider in providers {
        for root in provider.data_roots() {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(&root).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                count += 1;
                let metadata = entry.metadata().ok();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                entry.path().to_string_lossy().hash(&mut hasher);
                metadata
                    .as_ref()
                    .map(|metadata| metadata.len())
                    .hash(&mut hasher);
                paths = paths.wrapping_add(hasher.finish());
                if let Some(secs) = metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                {
                    if secs > latest {
                        latest = secs;
                    }
                }
            }
        }
    }
    (count, latest, paths)
}

/// Background poller that keeps the search index fresh without an external file-watch dependency.
/// When a watched root changes (a new/continued session in another terminal), it waits for writes
/// to settle, then rebuilds — near-real-time (a few seconds) and no app restart required.
///
/// The poll interval backs off while idle (no change) up to [`MAX_REFRESH_INTERVAL`] and snaps back
/// to [`MIN_REFRESH_INTERVAL`] the moment a change is seen. This keeps an actively-continued session
/// responsive while making a large history stop hammering CPU/IO (and the laptop battery) with a
/// full directory stat every few seconds once the user steps away.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
const MAX_REFRESH_INTERVAL: Duration = Duration::from_secs(64);
const WRITE_SETTLE_WINDOW: Duration = Duration::from_secs(16);

pub fn spawn_auto_refresh(engine: SharedSearchEngine, providers: Vec<Arc<dyn AgentProvider>>) {
    // Not "are there roots now?" — a provider can gain roots after startup (a WSL install is
    // adopted once discovery finishes), and a machine whose agents all live in WSL has none at
    // this point. Giving up here would leave exactly that machine without auto-refresh.
    std::thread::spawn(move || {
        // The initial index builder already walks the same roots. Wait for it to finish instead of
        // doubling startup disk traffic, which can starve page loads on large histories.
        while index_work_active() {
            std::thread::sleep(Duration::from_secs(2));
        }
        let mut last = roots_fingerprint(&providers);
        let mut interval = MIN_REFRESH_INTERVAL;
        let mut pending_change: Option<((u64, u64, u64), Instant)> = None;
        loop {
            std::thread::sleep(interval);
            let current = roots_fingerprint(&providers);
            if current == last {
                pending_change = None;
                // Nothing changed — wait longer next time so an idle app is nearly free.
                interval = (interval * 2).min(MAX_REFRESH_INTERVAL);
                continue;
            }

            // Do not reparse a large active rollout every eight seconds. Require the watched roots
            // to remain unchanged for a full settle window; each additional write resets it. This
            // keeps the UI responsive while Claude/Codex are actively appending to a session.
            interval = MIN_REFRESH_INTERVAL;
            match pending_change {
                Some((candidate, since))
                    if candidate == current && since.elapsed() >= WRITE_SETTLE_WINDOW => {}
                Some((candidate, _)) if candidate == current => continue,
                _ => {
                    pending_change = Some((current, Instant::now()));
                    continue;
                }
            }

            let Some(_work_guard) = try_begin_index_work() else {
                continue;
            };
            // Incremental: reuse unchanged sessions, re-parse only what changed on disk (vs the old
            // behavior of re-reading & re-parsing every session file on any change).
            let Ok((jieba, snapshot)) = engine
                .read()
                .map(|guard| (guard.jieba.clone(), guard.sessions_snapshot()))
            else {
                continue;
            };
            let prev = session_map_from(&snapshot);
            let (sessions, failed) =
                index_pool().install(|| build_sessions_incremental(&jieba, &providers, &prev));
            // Throttled: the in-memory index is updated every time, but the (potentially large)
            // on-disk cache is rewritten at most once per persist window, so frequent refreshes
            // during active work don't cause repeated full-file write amplification. Any drift is
            // reconciled from disk on the next cold start anyway.
            save_cached_sessions_throttled(&sessions);
            let built =
                index_pool().install(|| SearchEngine::from_sessions(jieba, sessions, failed));
            if let Ok(mut guard) = engine.write() {
                *guard = built;
            }
            last = roots_fingerprint(&providers);
            pending_change = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{IndexDoc, IndexText, TokenUsage};
    use jieba_rs::Jieba;
    use std::sync::Arc;

    fn doc(
        source: &str,
        session_id: &str,
        project: &str,
        modified: &str,
        texts: Vec<(&str, &str)>,
        tokens: TokenUsage,
        archive: Option<&str>,
    ) -> IndexDoc {
        IndexDoc {
            source: source.to_string(),
            session_id: session_id.to_string(),
            project: project.to_string(),
            project_path: format!("/{}", project),
            created_at: Some(modified.to_string()),
            updated_at: Some(modified.to_string()),
            agent_title: None,
            timestamp: Some(modified.to_string()),
            file_size_bytes: 1000,
            subagent_count: 0,
            archive_name: archive.map(|s| s.to_string()),
            first_prompt: Some("first".to_string()),
            model_contexts: Vec::new(),
            texts: texts
                .into_iter()
                .map(|(kind, text)| IndexText {
                    kind: kind.to_string(),
                    text: text.to_string(),
                })
                .collect(),
            tokens,
            key: format!("{}::{}", source, session_id),
            version: "1".to_string(),
        }
    }

    #[test]
    fn kind_code_and_scope_codes() {
        assert_eq!(kind_code("tool"), 1);
        assert_eq!(kind_code("reasoning"), 2);
        assert_eq!(kind_code("content"), 0);
        assert_eq!(kind_code("anything-else"), 0);

        let empty: Vec<String> = Vec::new();
        assert!(scope_codes(&empty).contains(&0)); // defaults to content

        let set = scope_codes(&["tool".to_string(), "reasoning".to_string()]);
        assert!(set.contains(&1) && set.contains(&2) && !set.contains(&0));
    }

    #[test]
    fn literal_query_detects_punctuation_only() {
        assert!(is_literal_query("192.168.0.1"));
        assert!(is_literal_query("foo-bar"));
        assert!(!is_literal_query("hello world"));
        assert!(!is_literal_query("变更记录")); // CJK is alphanumeric, not literal
    }

    #[test]
    fn snippet_centers_on_match_with_ellipses() {
        let text = format!("{}NEEDLE{}", "x".repeat(60), "y".repeat(200));
        let s = extract_snippet(&text, "needle");
        assert!(s.contains("NEEDLE"));
        assert!(s.starts_with("..."));
        assert!(s.ends_with("..."));
    }

    #[test]
    fn archive_scope_matching() {
        let some = Some("snap".to_string());
        assert!(ArchiveScope::from_value(Some("current")).matches(&None));
        assert!(!ArchiveScope::from_value(Some("current")).matches(&some));
        assert!(ArchiveScope::from_value(Some("archived")).matches(&some));
        assert!(!ArchiveScope::from_value(Some("archived")).matches(&None));
        assert!(ArchiveScope::from_value(Some("all")).matches(&None));
        assert!(ArchiveScope::from_value(Some("all")).matches(&some));
        assert!(matches!(
            ArchiveScope::from_value(None),
            ArchiveScope::Current
        ));
    }

    #[test]
    fn process_doc_bounds_preview_and_fast_index() {
        let jieba = Jieba::new();
        let content = (0..1500)
            .map(|index| format!("word{index:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        let meta = process_doc(
            &jieba,
            doc(
                "claude",
                "s1",
                "proj",
                "2024-01-01 00:00",
                vec![("content", &content)],
                TokenUsage::default(),
                None,
            ),
        );

        let preview_bytes: usize = meta.texts.iter().map(|text| text.text.len()).sum();
        assert!(
            preview_bytes <= PREVIEW_BUDGET_BYTES,
            "preview {preview_bytes} should not exceed budget {PREVIEW_BUDGET_BYTES}"
        );
        let content_tokens = meta
            .index_tokens
            .iter()
            .filter(|(code, _)| *code == 0)
            .count();
        assert_eq!(content_tokens, INDEX_TOKEN_BUDGETS[0]);
        assert!(
            !meta
                .index_tokens
                .iter()
                .any(|(_, token)| token.as_ref() == "word1499"),
            "late terms beyond the fast-index cap must not grow memory without bound"
        );
    }

    #[test]
    fn build_inverted_maps_tokens_to_all_sessions() {
        let jieba = Jieba::new();
        let a = process_doc(
            &jieba,
            doc(
                "claude",
                "a",
                "p",
                "2024-01-02 00:00",
                vec![("content", "alpha bravo")],
                TokenUsage::default(),
                None,
            ),
        );
        let b = process_doc(
            &jieba,
            doc(
                "codex",
                "b",
                "p",
                "2024-01-03 00:00",
                vec![("tool", "bravo charlie")],
                TokenUsage::default(),
                None,
            ),
        );
        let mut sessions = vec![a, b];
        let inverted = build_inverted(&mut sessions);
        assert!(inverted.contains_key("alpha"));
        assert!(inverted.contains_key("charlie"));
        assert_eq!(inverted["bravo"].len(), 2, "bravo appears in both sessions");
        let canonical = inverted
            .keys()
            .find(|token| token.as_ref() == "bravo")
            .expect("bravo key");
        for token in sessions
            .iter()
            .flat_map(|session| session.index_tokens.iter())
            .filter_map(|(_, token)| (token.as_ref() == "bravo").then_some(token))
        {
            assert!(
                Arc::ptr_eq(token, canonical),
                "session terms must share the inverted key allocation"
            );
        }
    }

    #[test]
    fn cache_rejects_old_schema_oversize_and_unbounded_tokens() {
        assert!(cache_file_size_allowed(MAX_CACHE_BYTES));
        assert!(!cache_file_size_allowed(MAX_CACHE_BYTES + 1));
        assert!(validated_cache_sessions(IndexCache {
            schema: CACHE_SCHEMA - 1,
            sessions: Vec::new(),
        })
        .is_none());

        let jieba = Jieba::new();
        let mut session = process_doc(
            &jieba,
            doc(
                "claude",
                "bounded",
                "p",
                "2024-01-01 00:00",
                vec![("content", "alpha")],
                TokenUsage::default(),
                None,
            ),
        );
        session.index_tokens = (0..=MAX_INDEX_TOKENS_PER_SESSION)
            .map(|index| (0, Arc::<str>::from(format!("token{index}"))))
            .collect();
        assert!(!cached_sessions_within_limits(&[session]));
    }

    #[test]
    fn json_cache_round_trip_preserves_bounded_arc_tokens() {
        let jieba = Jieba::new();
        let session = process_doc(
            &jieba,
            doc(
                "claude",
                "roundtrip",
                "p",
                "2024-01-01 00:00",
                vec![("content", "alpha bravo")],
                TokenUsage::default(),
                None,
            ),
        );
        let cache = IndexCache {
            schema: CACHE_SCHEMA,
            sessions: vec![session],
        };
        let bytes = serde_json::to_vec(&cache).expect("serialize cache");
        let decoded: IndexCache = serde_json::from_slice(&bytes).expect("deserialize cache");
        let sessions = validated_cache_sessions(decoded).expect("valid cache");
        assert!(sessions[0]
            .index_tokens
            .iter()
            .any(|(_, token)| token.as_ref() == "bravo"));
    }

    #[test]
    fn usage_summary_excludes_archived_and_sums_tokens() {
        let jieba = Arc::new(Jieba::new());
        let current = process_doc(
            &jieba,
            doc(
                "claude",
                "a",
                "projx",
                "2024-01-02 00:00",
                vec![("content", "hi")],
                TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_tokens: 1,
                    total_tokens: 16,
                },
                None,
            ),
        );
        let archived = process_doc(
            &jieba,
            doc(
                "claude",
                "b",
                "projx",
                "2024-01-02 00:00",
                vec![("content", "hi")],
                TokenUsage {
                    input_tokens: 20,
                    output_tokens: 0,
                    cache_tokens: 0,
                    total_tokens: 20,
                },
                Some("snap"),
            ),
        );
        let engine = SearchEngine::from_sessions(jieba.clone(), vec![current, archived], 0);
        let usage = engine.usage_summary();
        assert_eq!(usage.totals.sessions, 1, "archived sessions are excluded");
        assert_eq!(usage.totals.total_tokens, 16);
        assert_eq!(usage.totals.input_tokens, 10);
    }

    #[test]
    fn dashboard_summary_is_current_only_and_recency_sorted() {
        let jieba = Arc::new(Jieba::new());
        let older = process_doc(
            &jieba,
            doc(
                "claude",
                "old",
                "p",
                "2024-01-01 00:00",
                vec![("content", "a")],
                TokenUsage::default(),
                None,
            ),
        );
        let newer = process_doc(
            &jieba,
            doc(
                "claude",
                "new",
                "p",
                "2024-02-01 00:00",
                vec![("content", "a")],
                TokenUsage::default(),
                None,
            ),
        );
        let archived = process_doc(
            &jieba,
            doc(
                "claude",
                "arch",
                "p",
                "2024-03-01 00:00",
                vec![("content", "a")],
                TokenUsage::default(),
                Some("snap"),
            ),
        );
        let engine = SearchEngine::from_sessions(jieba.clone(), vec![older, newer, archived], 0);
        let dash = engine.dashboard_summary();
        assert_eq!(dash.total_sessions, 2, "archived excluded from dashboard");
        assert_eq!(
            dash.recent.first().map(|s| s.session_id.as_str()),
            Some("new"),
            "most-recent session is first"
        );
    }

    #[test]
    fn search_finds_by_token_and_browse_is_recency_sorted() {
        let jieba = Arc::new(Jieba::new());
        let a = process_doc(
            &jieba,
            doc(
                "claude",
                "a",
                "p",
                "2024-01-01 00:00",
                vec![("content", "alpha uniquetoken")],
                TokenUsage::default(),
                None,
            ),
        );
        let b = process_doc(
            &jieba,
            doc(
                "claude",
                "b",
                "p",
                "2024-02-01 00:00",
                vec![("content", "beta")],
                TokenUsage::default(),
                None,
            ),
        );
        let engine = SearchEngine::from_sessions(jieba.clone(), vec![a, b], 0);

        let hits = engine.search(
            "uniquetoken",
            10,
            &["content".to_string()],
            None,
            Some("current"),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session.session_id, "a");

        let browse = engine.browse_summaries(None, Some("current"));
        assert_eq!(browse.len(), 2);
        assert_eq!(
            browse[0].session_id, "b",
            "browse is sorted most-recent first"
        );
    }
}
