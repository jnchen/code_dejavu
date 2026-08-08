//! Codex CLI provider — `~/.codex`, date-bucketed `sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Codex rollouts wrap every record in a `{timestamp, type, payload}` envelope and carry two
//! overlapping streams: `response_item.*` (the API conversation) and `event_msg.*` (the TUI event
//! feed). We render the `response_item` stream as the single source of truth and drop the
//! `event_msg` duplicates — `function_call_output` already embeds the shell exit code / wall time /
//! output, so nothing is lost. Calls and results join on `call_id` exactly like Claude's
//! `tool_use_id`, so the existing frontend pairing works unchanged.

use super::{
    metadata_pool, quote_command_arg, AgentProvider, Capabilities, FastIndexTextCollector,
    IndexBatch, IndexDoc, IndexManifestEntry, InstructionCandidate, LineParser, TokenUsage,
};
use crate::error::AppError;
use crate::hosts::Host;
use crate::models::memory::{MemoryFile, MemoryFrontmatter, ProjectInfo};
use crate::models::profile::ProfileArchive;
use crate::models::rule::RuleFile;
use crate::models::session::{
    push_model_context, PaginatedRecords, SessionModelInfo, SessionRecord, SessionSearchHit,
    SessionSummary, SubagentInfo,
};
use crate::paths::app_data_dir;
use crate::services::jsonl;
use crate::services::profile_archiver::{self, SnapshotItem, SnapshotSpec};
use chrono::{DateTime, Local, Utc};
use rayon::prelude::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const FAST_META_SCAN_BYTES: u64 = 128 * 1024;

fn home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn fmt_ts(ts: &str) -> String {
    if let Ok(utc) = ts.parse::<DateTime<Utc>>() {
        let local: DateTime<Local> = utc.into();
        local.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        ts.replace('T', " ").chars().take(19).collect()
    }
}

fn fmt_epoch(value: i64) -> Option<String> {
    let dt = if value.abs() > 10_000_000_000 {
        chrono::DateTime::from_timestamp_millis(value)
    } else {
        chrono::DateTime::from_timestamp(value, 0)
    }?;
    let local: DateTime<Local> = dt.into();
    Some(local.format("%Y-%m-%d %H:%M:%S").to_string())
}

#[derive(Debug, Clone, Default)]
struct CodexThreadInfo {
    title: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

fn codex_memory_fallback_title(thread_id: &str, rollout_summary: &str, raw_memory: &str) -> String {
    for body in [rollout_summary, raw_memory] {
        for line in body.lines() {
            let candidate = line
                .trim()
                .trim_start_matches('#')
                .trim_start_matches(['-', '*'])
                .trim();
            if candidate.is_empty()
                || candidate == "---"
                || candidate.starts_with('<')
                || candidate.len() > 300
            {
                continue;
            }
            let title: String = candidate.chars().take(100).collect();
            if !title.is_empty() {
                return title;
            }
        }
    }
    let short_id: String = thread_id.chars().take(8).collect();
    format!("Codex 会话 {}", short_id)
}

fn readable_session_title(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() || value.contains('\u{fffd}') {
        return None;
    }
    let total = value.chars().count();
    let question_marks = value.chars().filter(|character| *character == '?').count();
    if question_marks >= 3 && question_marks.saturating_mul(5) >= total {
        return None;
    }
    Some(value)
}

/// Cheap content version for incremental indexing: file size + mtime (seconds).
fn file_version_meta(meta: &fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}:{}", meta.len(), mtime)
}

fn codex_effort(payload: &Value) -> Option<String> {
    payload.get("effort").and_then(|e| {
        e.as_str()
            .map(String::from)
            .or_else(|| e.get("effort").and_then(|x| x.as_str()).map(String::from))
    })
}

fn token_usage_from_value(total: &Value) -> TokenUsage {
    let get = |key: &str| total.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    TokenUsage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_tokens: get("cached_input_tokens"),
        total_tokens: get("total_tokens"),
    }
}

fn token_usage_delta(total: TokenUsage, baseline: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: total.input_tokens.saturating_sub(baseline.input_tokens),
        output_tokens: total.output_tokens.saturating_sub(baseline.output_tokens),
        cache_tokens: total.cache_tokens.saturating_sub(baseline.cache_tokens),
        total_tokens: total.total_tokens.saturating_sub(baseline.total_tokens),
    }
}

#[derive(Debug, Clone)]
struct CodexRolloutMeta {
    /// The physical thread represented by this rollout (`payload.id`, also the filename UUID).
    thread_id: String,
    /// Root user thread hint carried by modern subagent rollouts (`payload.session_id`).
    logical_session_id: Option<String>,
    /// Direct parent thread. This differs from `logical_session_id` for nested subagents.
    parent_thread_id: Option<String>,
    is_subagent: bool,
    cwd: String,
    started: Option<String>,
    model_contexts: Vec<SessionModelInfo>,
    agent_path: Option<String>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
}

#[derive(Debug, Clone)]
struct RolloutEntry {
    path: PathBuf,
    meta: CodexRolloutMeta,
    file_size: u64,
    version: String,
}

#[derive(Debug, Clone)]
struct RolloutGroup {
    root: RolloutEntry,
    descendants: Vec<RolloutEntry>,
}

impl RolloutGroup {
    fn members(&self) -> impl Iterator<Item = &RolloutEntry> {
        std::iter::once(&self.root).chain(self.descendants.iter())
    }

    fn file_size(&self) -> u64 {
        self.members()
            .fold(0, |total, entry| total.saturating_add(entry.file_size))
    }

    fn model_contexts(&self) -> Vec<SessionModelInfo> {
        let mut out = Vec::new();
        for entry in self.members() {
            for context in &entry.meta.model_contexts {
                if !out.contains(context) {
                    out.push(context.clone());
                }
            }
        }
        out
    }

    /// A child append/add/remove must invalidate the root document in the incremental index.
    fn composite_version(&self) -> String {
        let mut parts: Vec<String> = self
            .members()
            .map(|entry| format!("{}@{}", entry.path.to_string_lossy(), entry.version))
            .collect();
        parts.sort();
        parts.join("|")
    }
}

pub struct CodexProvider {
    codex_dir: PathBuf,
    sessions_dir: PathBuf,
    memories_db: PathBuf,
    archive_root: PathBuf,
    /// The machine this install lives on. Rollouts record `cwd` as the agent saw it, so a WSL
    /// install's project paths need translating before anything here can open them.
    host: Host,
    project_inventory_cache: Mutex<Option<CodexProjectInventory>>,
}

#[derive(Clone)]
struct CodexProjectInventory {
    created: Instant,
    roots: Vec<PathBuf>,
    session_count: u32,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    pub fn new() -> Self {
        Self::for_host(Host::Native, &home())
    }

    /// A Codex install rooted at `home`, which may belong to another host (e.g. a WSL distro).
    /// Snapshots stay in the app's own data directory, namespaced per host so two installs cannot
    /// overwrite each other's archives.
    pub fn for_host(host: Host, home: &Path) -> Self {
        let codex_dir = home.join(".codex");
        let archive_root = match host.tag() {
            Some(_) => app_data_dir()
                .join("archives")
                .join("codex")
                .join(format!("wsl-{}", host.key())),
            None => app_data_dir().join("archives").join("codex"),
        };
        Self {
            sessions_dir: codex_dir.join("sessions"),
            memories_db: codex_dir.join("memories_1.sqlite"),
            archive_root,
            host,
            project_inventory_cache: Mutex::new(None),
            codex_dir,
        }
    }

    fn rollout_files_in(root: &Path) -> Vec<PathBuf> {
        if !root.exists() {
            return Vec::new();
        }
        WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .filter(|p| {
                p.extension().is_some_and(|x| x == "jsonl")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("rollout-"))
            })
            .collect()
    }

    fn archive_sessions_dir(&self, archive_name: &str) -> PathBuf {
        self.archive_root
            .join(archive_name)
            .join("codex")
            .join("sessions")
    }

    fn rollout_sources(&self) -> Vec<(PathBuf, Option<String>)> {
        let mut sources = Vec::new();
        if self.sessions_dir.exists() {
            sources.push((self.sessions_dir.clone(), None));
        }
        if let Ok(archives) = fs::read_dir(&self.archive_root) {
            for entry in archives.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let archive_name = entry.file_name().to_string_lossy().to_string();
                let sessions_dir = entry.path().join("codex").join("sessions");
                if sessions_dir.exists() {
                    sources.push((sessions_dir, Some(archive_name)));
                }
            }
        }
        sources
    }

    /// Read a rollout's identity from its FIRST `session_meta`. Forked subagent rollouts may embed
    /// the parent's `session_meta` later in the file, so allowing a later record to overwrite this
    /// one turns the child back into a second copy of the root thread.
    fn read_meta_with_contexts(path: &Path, include_contexts: bool) -> Option<CodexRolloutMeta> {
        let file = File::open(path).ok()?;
        // Fast metadata callers (rules, instructions, memory-project count, manifests) must never
        // turn a malformed/legacy rollout into a multi-gigabyte read. Valid Codex rollouts place
        // `session_meta` in the first JSONL record; the bounded path intentionally skips files
        // that do not expose it near the front. Full session parsing remains unbounded below.
        let reader = BufReader::with_capacity(64 * 1024, file);
        let reader = reader.take(if include_contexts {
            u64::MAX
        } else {
            FAST_META_SCAN_BYTES
        });
        let mut meta: Option<CodexRolloutMeta> = None;
        let mut provider = None;
        let mut contexts = Vec::new();

        for line in reader.lines().map_while(Result::ok) {
            if !line.contains("\"session_meta\"") && !line.contains("\"turn_context\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(t) = v.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(pl) = v.get("payload") else { continue };

            match t {
                "session_meta" => {
                    if meta.is_some() {
                        continue;
                    }
                    let clean_field = |key: &str| {
                        pl.get(key)
                            .and_then(|x| x.as_str())
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .map(String::from)
                    };
                    let logical_session_id = clean_field("session_id");
                    let thread_id = clean_field("id").or_else(|| logical_session_id.clone())?;
                    let spawn = pl.pointer("/source/subagent/thread_spawn");
                    let nested_field = |key: &str| {
                        spawn
                            .and_then(|s| s.get(key))
                            .and_then(|x| x.as_str())
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .map(String::from)
                    };
                    let mut parent_thread_id = clean_field("parent_thread_id")
                        .or_else(|| nested_field("parent_thread_id"));
                    let has_subagent_source = pl
                        .get("source")
                        .and_then(|source| source.get("subagent"))
                        .is_some();
                    let is_subagent = clean_field("thread_source").as_deref() == Some("subagent")
                        || has_subagent_source
                        || parent_thread_id.is_some()
                        || logical_session_id
                            .as_deref()
                            .is_some_and(|root| root != thread_id);
                    // A normal user-created fork can carry `forked_from_id`. It becomes a parent
                    // fallback only after independent metadata has established this is a subagent.
                    if is_subagent && parent_thread_id.is_none() {
                        parent_thread_id = clean_field("forked_from_id");
                    }
                    let started = pl
                        .get("timestamp")
                        .and_then(|x| x.as_str())
                        .or_else(|| v.get("timestamp").and_then(|x| x.as_str()))
                        .map(fmt_ts);
                    provider = pl
                        .get("model_provider")
                        .and_then(|x| x.as_str())
                        .map(String::from);
                    meta = Some(CodexRolloutMeta {
                        thread_id,
                        logical_session_id,
                        parent_thread_id,
                        is_subagent,
                        cwd: clean_field("cwd").unwrap_or_default(),
                        started,
                        model_contexts: Vec::new(),
                        agent_path: clean_field("agent_path")
                            .or_else(|| nested_field("agent_path")),
                        agent_nickname: clean_field("agent_nickname")
                            .or_else(|| nested_field("agent_nickname")),
                        agent_role: clean_field("agent_role")
                            .or_else(|| nested_field("agent_role")),
                    });
                    if !include_contexts {
                        break;
                    }
                }
                "turn_context" => {
                    push_model_context(
                        &mut contexts,
                        provider.clone(),
                        pl.get("model").and_then(|x| x.as_str()).map(String::from),
                        codex_effort(pl),
                    );
                }
                _ => {}
            }
        }

        if contexts.is_empty() {
            push_model_context(&mut contexts, provider, None, None);
        }
        let mut meta = meta?;
        meta.model_contexts = contexts;
        Some(meta)
    }

    fn read_meta(path: &Path) -> Option<CodexRolloutMeta> {
        Self::read_meta_with_contexts(path, true)
    }

    fn read_meta_fast(path: &Path) -> Option<CodexRolloutMeta> {
        Self::read_meta_with_contexts(path, false)
    }

    fn rollout_entry(path: PathBuf, include_contexts: bool) -> Option<RolloutEntry> {
        let meta = if include_contexts {
            Self::read_meta(&path)?
        } else {
            Self::read_meta_fast(&path)?
        };
        let file_meta = fs::metadata(&path).ok();
        Some(RolloutEntry {
            path,
            meta,
            file_size: file_meta.as_ref().map(|m| m.len()).unwrap_or(0),
            version: file_meta
                .as_ref()
                .map(file_version_meta)
                .unwrap_or_default(),
        })
    }

    fn rollout_entries_in_with_mode(
        root: &Path,
        include_contexts: bool,
        parallel: bool,
    ) -> Vec<RolloutEntry> {
        let files = Self::rollout_files_in(root);
        if parallel {
            files
                .into_par_iter()
                .filter_map(|path| Self::rollout_entry(path, include_contexts))
                .collect()
        } else {
            files
                .into_iter()
                .filter_map(|path| Self::rollout_entry(path, include_contexts))
                .collect()
        }
    }

    fn resolve_root_index(
        start: usize,
        entries: &[RolloutEntry],
        by_thread: &HashMap<String, usize>,
    ) -> usize {
        if !entries[start].meta.is_subagent {
            return start;
        }

        let mut current = start;
        let mut highest_existing = start;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current) {
                // Corrupt/cyclic metadata: keep the original rollout visible rather than hiding it.
                return start;
            }
            let meta = &entries[current].meta;
            if !meta.is_subagent {
                return current;
            }

            if let Some(root_hint) = meta.logical_session_id.as_deref() {
                if root_hint != meta.thread_id {
                    if let Some(&root_idx) = by_thread.get(root_hint) {
                        if !entries[root_idx].meta.is_subagent {
                            return root_idx;
                        }
                    }
                }
            }

            let Some(parent_id) = meta.parent_thread_id.as_deref() else {
                return highest_existing;
            };
            let Some(&parent_idx) = by_thread.get(parent_id) else {
                return highest_existing;
            };
            highest_existing = parent_idx;
            current = parent_idx;
        }
    }

    fn rollout_groups_in_with_contexts(root: &Path, include_contexts: bool) -> Vec<RolloutGroup> {
        Self::rollout_groups_in_with_contexts_mode(root, include_contexts, true)
    }

    fn rollout_groups_in_with_contexts_mode(
        root: &Path,
        include_contexts: bool,
        parallel: bool,
    ) -> Vec<RolloutGroup> {
        let entries = Self::rollout_entries_in_with_mode(root, include_contexts, parallel);
        let mut by_thread = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            by_thread
                .entry(entry.meta.thread_id.clone())
                .or_insert(index);
        }

        let mut members_by_root: HashMap<usize, Vec<usize>> = HashMap::new();
        for index in 0..entries.len() {
            let root = Self::resolve_root_index(index, &entries, &by_thread);
            members_by_root.entry(root).or_default().push(index);
        }

        let mut groups = Vec::with_capacity(members_by_root.len());
        for (root_index, member_indices) in members_by_root {
            let mut descendants: Vec<RolloutEntry> = member_indices
                .into_iter()
                .filter(|&index| index != root_index)
                .map(|index| entries[index].clone())
                .collect();
            descendants.sort_by(|a, b| {
                a.meta
                    .started
                    .cmp(&b.meta.started)
                    .then_with(|| a.meta.thread_id.cmp(&b.meta.thread_id))
            });
            groups.push(RolloutGroup {
                root: entries[root_index].clone(),
                descendants,
            });
        }
        groups
    }

    fn rollout_groups_in(root: &Path) -> Vec<RolloutGroup> {
        Self::rollout_groups_in_with_contexts(root, true)
    }

    fn rollout_groups_fast_in(root: &Path) -> Vec<RolloutGroup> {
        Self::rollout_groups_in_with_contexts(root, false)
    }

    fn groups_for_archive(&self, archive: Option<&str>) -> Vec<RolloutGroup> {
        let sessions_dir = match archive.filter(|name| !name.trim().is_empty()) {
            Some(name) => self.archive_sessions_dir(name),
            None => self.sessions_dir.clone(),
        };
        Self::rollout_groups_fast_in(&sessions_dir)
    }

    fn subagent_links(group: &RolloutGroup) -> HashMap<String, (String, Option<Value>)> {
        let mut spawn_args: HashMap<String, Value> = HashMap::new();
        let mut call_by_child: HashMap<String, String> = HashMap::new();
        let mut parent_threads: HashSet<String> = group
            .descendants
            .iter()
            .filter_map(|child| child.meta.parent_thread_id.clone())
            .collect();
        parent_threads.insert(group.root.meta.thread_id.clone());

        // A depth-2 child's `started` event lives in its direct parent's rollout. Scan only members
        // that actually parent another thread: depth-1-only groups now read the root once instead
        // of rereading every fork's inherited history.
        for entry in group
            .members()
            .filter(|entry| parent_threads.contains(&entry.meta.thread_id))
        {
            let Ok(file) = File::open(&entry.path) else {
                continue;
            };
            for line in BufReader::with_capacity(64 * 1024, file)
                .lines()
                .map_while(Result::ok)
            {
                if !line.contains("\"spawn_agent\"")
                    && !line.contains("\"sub_agent_activity\"")
                    && !line.contains("agent_id")
                {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let envelope_type = value.get("type").and_then(|x| x.as_str());
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                let payload_type = payload.get("type").and_then(|x| x.as_str());
                if envelope_type == Some("response_item")
                    && payload_type == Some("function_call")
                    && payload.get("name").and_then(|x| x.as_str()) == Some("spawn_agent")
                {
                    if let Some(call_id) = payload.get("call_id").and_then(|x| x.as_str()) {
                        let arguments = payload
                            .get("arguments")
                            .and_then(|x| x.as_str())
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                            .unwrap_or(Value::Null);
                        spawn_args.entry(call_id.to_string()).or_insert(arguments);
                    }
                } else if envelope_type == Some("response_item")
                    && payload_type == Some("function_call_output")
                {
                    let Some(call_id) = payload.get("call_id").and_then(|x| x.as_str()) else {
                        continue;
                    };
                    if !spawn_args.contains_key(call_id) {
                        continue;
                    }
                    let output = match payload.get("output") {
                        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).ok(),
                        Some(value) => Some(value.clone()),
                        None => None,
                    };
                    if let Some(agent_id) = output
                        .as_ref()
                        .and_then(|value| value.get("agent_id"))
                        .and_then(|x| x.as_str())
                    {
                        call_by_child
                            .entry(agent_id.to_string())
                            .or_insert_with(|| call_id.to_string());
                    }
                } else if envelope_type == Some("event_msg")
                    && payload_type == Some("sub_agent_activity")
                    && payload.get("kind").and_then(|x| x.as_str()) == Some("started")
                {
                    if let (Some(child), Some(event_id)) = (
                        payload.get("agent_thread_id").and_then(|x| x.as_str()),
                        payload.get("event_id").and_then(|x| x.as_str()),
                    ) {
                        call_by_child
                            .entry(child.to_string())
                            .or_insert_with(|| event_id.to_string());
                    }
                }
            }
        }

        let mut links = HashMap::new();
        for child in &group.descendants {
            let mut call_id = call_by_child
                .get(&child.meta.thread_id)
                .cloned()
                .unwrap_or_default();
            if call_id.is_empty() {
                // Older rollouts may lack activity events. Canonical agent_path `/root/task`
                // still gives us a conservative task_name fallback.
                if let Some(path) = child.meta.agent_path.as_deref() {
                    if let Some((id, _)) = spawn_args.iter().find(|(_, args)| {
                        args.get("task_name")
                            .and_then(|x| x.as_str())
                            .is_some_and(|task| path == task || path.ends_with(&format!("/{task}")))
                    }) {
                        call_id = id.clone();
                    }
                }
            }
            let args = spawn_args.get(&call_id).cloned();
            links.insert(child.meta.thread_id.clone(), (call_id, args));
        }
        links
    }

    fn validated_subagent_path(
        &self,
        session_id: &str,
        agent_id: &str,
        archive: Option<&str>,
    ) -> Option<PathBuf> {
        // Validate only the requested threads' parent chains instead of rebuilding every rollout
        // group for each page. The resolver mirrors `resolve_root_index`, including orphan/cycle
        // promotion, while keeping current and archive domains separate through `session_file`.
        let root_path = self.session_file(session_id, archive)?;
        let root_meta = Self::read_meta_fast(&root_path)?;
        if self.resolved_root_thread(root_meta, archive)? != session_id {
            return None;
        }
        let path = self.session_file(agent_id, archive)?;
        let meta = Self::read_meta_fast(&path)?;
        if !meta.is_subagent || meta.thread_id == session_id {
            return None;
        }
        (self.resolved_root_thread(meta, archive)?.as_str() == session_id).then_some(path)
    }

    fn resolved_root_thread(
        &self,
        start: CodexRolloutMeta,
        archive: Option<&str>,
    ) -> Option<String> {
        let original = start.thread_id.clone();
        let mut current = start;
        let mut highest_existing = original.clone();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.thread_id.clone()) {
                return Some(original);
            }
            if !current.is_subagent {
                return Some(current.thread_id);
            }
            if let Some(root_hint) = current.logical_session_id.as_deref() {
                if root_hint != current.thread_id {
                    if let Some(root_path) = self.session_file(root_hint, archive) {
                        if let Some(root_meta) = Self::read_meta_fast(&root_path) {
                            if !root_meta.is_subagent {
                                return Some(root_meta.thread_id);
                            }
                        }
                    }
                }
            }
            let Some(parent_id) = current.parent_thread_id.as_deref() else {
                return Some(highest_existing);
            };
            let Some(parent_path) = self.session_file(parent_id, archive) else {
                return Some(highest_existing);
            };
            let Some(parent_meta) = Self::read_meta_fast(&parent_path) else {
                return Some(highest_existing);
            };
            highest_existing = parent_meta.thread_id.clone();
            current = parent_meta;
        }
    }

    /// Locate a rollout file by its session uuid (filename ends with `<uuid>.jsonl`).
    fn session_file(&self, session_id: &str, archive: Option<&str>) -> Option<PathBuf> {
        let needle = format!("{}.jsonl", session_id);
        let sessions_dir = match archive.filter(|name| !name.trim().is_empty()) {
            Some(name) => self.archive_sessions_dir(name),
            None => self.sessions_dir.clone(),
        };
        Self::rollout_files_in(&sessions_dir).into_iter().find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&needle))
        })
    }

    fn config_file(&self) -> PathBuf {
        self.codex_dir.join("config.toml")
    }

    fn instruction_file_in(codex_dir: &Path) -> PathBuf {
        // Codex installations in the wild use both spellings. Preserve the existing filename on
        // case-sensitive systems; default to the lowercase name used by current installations.
        let entries: Vec<_> = fs::read_dir(codex_dir)
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        for filename in ["instruction.md", "Instruction.md", "INSTRUCTION.md"] {
            if let Some(entry) = entries.iter().find(|entry| entry.file_name() == filename) {
                return entry.path();
            }
        }
        if let Some(entry) = entries.iter().find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("instruction.md")
        }) {
            return entry.path();
        }
        codex_dir.join("instruction.md")
    }

    fn instruction_file(&self) -> PathBuf {
        Self::instruction_file_in(&self.codex_dir)
    }

    fn thread_info(&self, archive: Option<&str>) -> HashMap<String, CodexThreadInfo> {
        let db = match archive {
            Some(name) => self
                .archive_root
                .join(name)
                .join("codex")
                .join("state_5.sqlite"),
            None => self.codex_dir.join("state_5.sqlite"),
        };
        if !db.exists() {
            return HashMap::new();
        }
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return HashMap::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT id, title, created_at, updated_at FROM threads")
        else {
            return HashMap::new();
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        }) else {
            return HashMap::new();
        };
        rows.flatten()
            .map(|(id, title, created, updated)| {
                (
                    id,
                    CodexThreadInfo {
                        title: title.and_then(readable_session_title),
                        created_at: created.and_then(fmt_epoch),
                        updated_at: updated.and_then(fmt_epoch),
                    },
                )
            })
            .collect()
    }

    fn apply_thread_info(doc: &mut IndexDoc, info: Option<&CodexThreadInfo>) {
        if let Some(info) = info {
            doc.agent_title = info.title.clone();
            doc.created_at = info.created_at.clone().or_else(|| doc.created_at.clone());
            doc.updated_at = info.updated_at.clone().or_else(|| doc.updated_at.clone());
            doc.timestamp = doc.updated_at.clone();
        }
    }

    fn memory_titles(&self) -> HashMap<String, String> {
        let Ok(conn) = self.memory_conn() else {
            return HashMap::new();
        };
        let Ok(mut statement) = conn.prepare(
            "SELECT thread_id, substr(raw_memory, 1, 2000), \
                 substr(rollout_summary, 1, 2000) FROM stage1_outputs",
        ) else {
            return HashMap::new();
        };
        let Ok(rows) = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) else {
            return HashMap::new();
        };
        rows.flatten()
            .map(|(thread_id, raw, summary)| {
                let title = codex_memory_fallback_title(&thread_id, &summary, &raw);
                (thread_id, title)
            })
            .collect()
    }

    fn thread_version(info: Option<&CodexThreadInfo>) -> String {
        let Some(info) = info else {
            return String::new();
        };
        format!(
            "|thread:{}:{}:{}",
            info.title.as_deref().unwrap_or(""),
            info.created_at.as_deref().unwrap_or(""),
            info.updated_at.as_deref().unwrap_or("")
        )
    }

    fn snapshot_spec(&self) -> SnapshotSpec {
        SnapshotSpec {
            source: "codex",
            display_name: "Codex CLI",
            archive_root: self.archive_root.clone(),
            items: vec![SnapshotItem {
                name: "codex",
                path: self.codex_dir.clone(),
                preserve: &["auth.json", "cap_sid"],
            }],
            clear_current_on_create: true,
        }
    }

    fn memory_conn(&self) -> Result<rusqlite::Connection, AppError> {
        rusqlite::Connection::open_with_flags(
            &self.memories_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| AppError::Archive(format!("open codex memories: {}", e)))
    }

    fn codex_memory_count(&self) -> u32 {
        if !self.memories_db.exists() {
            return 0;
        }
        self.memory_conn()
            .and_then(|conn| {
                conn.query_row("SELECT count(*) FROM stage1_outputs", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| AppError::Archive(e.to_string()))
            })
            .unwrap_or(0)
            .max(0) as u32
    }

    fn scan_live_project_inventory(&self) -> CodexProjectInventory {
        // This runs while the inventory cache mutex is held to coalesce concurrent page loads.
        // Use the dedicated metadata pool: waiters may themselves be Rayon workers, so nesting
        // into the global pool here can deadlock under rapid menu navigation.
        let groups = metadata_pool().install(|| Self::rollout_groups_fast_in(&self.sessions_dir));
        let session_count = groups.len().min(u32::MAX as usize) as u32;
        let mut roots: Vec<PathBuf> = groups
            .into_iter()
            .filter_map(|group| {
                let cwd = group.root.meta.cwd.trim();
                (!cwd.is_empty()).then(|| self.host.to_readable(cwd))
            })
            .collect();
        roots.sort();
        roots.dedup();
        CodexProjectInventory {
            created: Instant::now(),
            roots,
            session_count,
        }
    }

    fn live_project_inventory(&self) -> CodexProjectInventory {
        const TTL: Duration = Duration::from_secs(30);
        if let Ok(mut cache) = self.project_inventory_cache.lock() {
            if let Some(inventory) = cache.as_ref() {
                if inventory.created.elapsed() < TTL {
                    return inventory.clone();
                }
            }
            // Keep the guard while scanning so concurrent rule/instruction/memory requests share
            // one metadata pass instead of opening every rollout independently.
            let inventory = self.scan_live_project_inventory();
            *cache = Some(inventory.clone());
            return inventory;
        }
        self.scan_live_project_inventory()
    }

    fn live_session_count_fast(&self) -> u32 {
        self.live_project_inventory().session_count
    }

    /// Parse a single rollout file into an [`IndexDoc`]. Returns `(doc, failed)` where `failed` is
    /// true only on a real read/parse error (a session with no indexable text is a silent skip,
    /// not a failure). Shared by the full and incremental index paths.
    fn index_one(&self, path: &Path, archive_name: Option<String>) -> (Option<IndexDoc>, bool) {
        let Some(identity) = Self::read_meta_fast(path) else {
            return (None, true);
        };
        let is_subagent = identity.is_subagent;
        let agent_path = identity.agent_path.clone();
        let Ok(file) = File::open(path) else {
            return (None, true);
        };
        let reader = BufReader::with_capacity(128 * 1024, file);
        let mut texts = FastIndexTextCollector::default();
        let mut first_prompt: Option<String> = None;
        let mut tokens = TokenUsage::default();
        let mut updated_at = None;
        let mut subagent_baseline: Option<TokenUsage> = None;
        for line in reader.lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let pl = v.get("payload");
            let pt = pl
                .and_then(|p| p.get("type"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if matches!(
                (t, pt),
                ("response_item", "message") | ("response_item", "agent_message")
            ) {
                if let Some(ts) = v.get("timestamp").and_then(|value| value.as_str()) {
                    updated_at = Some(fmt_ts(ts));
                }
            }
            match (t, pt) {
                ("response_item", "message") => {
                    let role = pl
                        .and_then(|p| p.get("role"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    let text = join_text(pl.and_then(|p| p.get("content")));
                    if role != "user" && role != "assistant" {
                        continue;
                    }
                    if text.trim().is_empty()
                        || (role == "user" && text.trim_start().starts_with('<'))
                    {
                        continue;
                    }
                    if first_prompt.is_none() && role == "user" {
                        first_prompt = Some(text.chars().take(200).collect());
                    }
                    push_text(&mut texts, "content", text);
                }
                ("response_item", "agent_message")
                    if is_subagent
                        && subagent_baseline.is_none()
                        && pl.and_then(|p| p.get("recipient")).and_then(|x| x.as_str())
                            == agent_path.as_deref() =>
                {
                    // v2 forks physically copy selected parent history, including cumulative token
                    // totals. The runtime then addresses the real assignment to this exact agent
                    // path. Snapshot the LAST inherited total there (not max: parallel history can
                    // arrive out of order) so group usage adds only the child's own delta.
                    subagent_baseline = Some(tokens);
                }
                ("response_item", "function_call") => {
                    if let Some(cmd) = pl
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.as_str())
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|a| a.get("command").and_then(|c| c.as_str()).map(String::from))
                    {
                        push_text(&mut texts, "tool", cmd);
                    }
                }
                ("response_item", "function_call_output")
                | ("response_item", "custom_tool_call_output") => {
                    let out = match pl.and_then(|p| p.get("output")) {
                        Some(Value::String(s)) => s.clone(),
                        Some(o) => o.to_string(),
                        None => String::new(),
                    };
                    push_text(&mut texts, "tool", out);
                }
                ("response_item", "custom_tool_call") => {
                    if let Some(inp) = pl.and_then(|p| p.get("input")).and_then(|x| x.as_str()) {
                        push_text(&mut texts, "tool", inp.to_string());
                    }
                }
                ("event_msg", "agent_reasoning") => {
                    if let Some(rt) = pl.and_then(|p| p.get("text")).and_then(|x| x.as_str()) {
                        push_text(&mut texts, "reasoning", rt.to_string());
                    }
                }
                ("event_msg", "token_count") => {
                    // `total_token_usage` is cumulative for the session — last event wins.
                    if let Some(total) = pl
                        .and_then(|p| p.get("info"))
                        .and_then(|i| i.get("total_token_usage"))
                    {
                        tokens = token_usage_from_value(total);
                    }
                }
                _ => {}
            }
        }
        if let Some(baseline) = subagent_baseline {
            // A reset/counter-version change is safer interpreted as an independent child total.
            if tokens.total_tokens >= baseline.total_tokens {
                tokens = token_usage_delta(tokens, baseline);
            }
        }
        // Keep content-less sessions in the index too, so an empty/aborted rollout still shows up in
        // the browse list (matching Claude and the pre-index disk-scan behavior). Only a genuine
        // read/parse error below counts as a failure.
        let Some(meta) = Self::read_meta(path) else {
            return (None, true);
        };
        let file_meta = fs::metadata(path).ok();
        let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let version = file_meta
            .as_ref()
            .map(file_version_meta)
            .unwrap_or_default();
        (
            Some(IndexDoc {
                source: "codex".to_string(),
                session_id: meta.thread_id,
                project: meta.cwd.clone(),
                project_path: self
                    .host
                    .to_readable(&meta.cwd)
                    .to_string_lossy()
                    .to_string(),
                created_at: meta.started.clone(),
                updated_at: updated_at.clone().or_else(|| meta.started.clone()),
                agent_title: None,
                timestamp: updated_at.or(meta.started),
                file_size_bytes: file_size,
                subagent_count: 0,
                archive_name,
                first_prompt,
                model_contexts: meta.model_contexts,
                texts: texts.into_texts(),
                tokens,
                key: path.to_string_lossy().to_string(),
                version,
            }),
            false,
        )
    }

    fn index_group(
        &self,
        group: &RolloutGroup,
        archive_name: Option<String>,
    ) -> (Option<IndexDoc>, usize) {
        let (root_doc, root_failed) = self.index_one(&group.root.path, archive_name.clone());
        let Some(mut root_doc) = root_doc else {
            return (None, usize::from(root_failed));
        };
        let mut failed = usize::from(root_failed);

        // Keep child-only work globally searchable while emitting just one result card for the
        // logical root session. Forked parent history can repeat tokens, but the search engine
        // deduplicates per-session index tokens; the bounded preview budget limits stored snippets.
        for child in &group.descendants {
            let (doc, child_failed) = self.index_one(&child.path, archive_name.clone());
            failed += usize::from(child_failed);
            if let Some(mut doc) = doc {
                root_doc.texts.append(&mut doc.texts);
                root_doc.tokens.input_tokens = root_doc
                    .tokens
                    .input_tokens
                    .saturating_add(doc.tokens.input_tokens);
                root_doc.tokens.output_tokens = root_doc
                    .tokens
                    .output_tokens
                    .saturating_add(doc.tokens.output_tokens);
                root_doc.tokens.cache_tokens = root_doc
                    .tokens
                    .cache_tokens
                    .saturating_add(doc.tokens.cache_tokens);
                root_doc.tokens.total_tokens = root_doc
                    .tokens
                    .total_tokens
                    .saturating_add(doc.tokens.total_tokens);
            }
        }
        root_doc.file_size_bytes = group.file_size();
        root_doc.subagent_count = group.descendants.len().min(u32::MAX as usize) as u32;
        root_doc.model_contexts = group.model_contexts();
        root_doc.key = group.root.path.to_string_lossy().to_string();
        root_doc.version = group.composite_version();
        (Some(root_doc), failed)
    }
}

impl AgentProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }
    fn available(&self) -> bool {
        self.codex_dir.exists()
    }
    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.sessions_dir.clone(), self.archive_root.clone()]
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions_read: true,
            sessions_search: true,
            sessions_resume: true,
            sessions_subagents: true,
            rules_read: true,
            rules_write: false,
            memory_read: self.memories_db.exists(),
            memory_write: false,
            instructions_read: true,
            instructions_write: true,
            archive_read: true,
            archive_write: true,
            config_format: "toml",
        }
    }

    fn list_profiles(&self) -> Result<Vec<ProfileArchive>, AppError> {
        profile_archiver::list_profiles(&self.snapshot_spec())
    }

    fn create_profile(&self, name: Option<String>) -> Result<ProfileArchive, AppError> {
        profile_archiver::create_profile(&self.snapshot_spec(), name)
    }

    fn restore_profile(&self, name: &str) -> Result<(), AppError> {
        profile_archiver::restore_profile(&self.snapshot_spec(), name)
    }

    fn delete_profile(&self, name: &str) -> Result<(), AppError> {
        profile_archiver::delete_profile(&self.snapshot_spec(), name)
    }

    fn rename_profile(&self, old_name: &str, new_name: &str) -> Result<(), AppError> {
        profile_archiver::rename_profile(&self.snapshot_spec(), old_name, new_name)
    }

    fn global_instruction_candidates(&self) -> Vec<InstructionCandidate> {
        vec![
            InstructionCandidate {
                title: "全局 Instruction.md".to_string(),
                scope: "global",
                kind: "instructions",
                path: self.instruction_file(),
                editable: true,
                include_missing: true,
                exists: None,
                size_bytes: None,
                description: "Codex CLI 全局自定义指令。".to_string(),
            },
            InstructionCandidate {
                title: "全局 config.toml".to_string(),
                scope: "global",
                kind: "config",
                path: self.config_file(),
                editable: true,
                include_missing: true,
                exists: None,
                size_bytes: None,
                description: "Codex CLI 全局配置。".to_string(),
            },
        ]
    }

    fn list_sessions(&self, project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
        let mut sessions = Vec::new();
        let thread_info = self.thread_info(None);
        let memory_titles = self.memory_titles();
        let groups = Self::rollout_groups_in(&self.sessions_dir);
        for group in groups {
            let meta = &group.root.meta;
            if let Some(p) = project {
                if meta.cwd != p {
                    continue;
                }
            }
            let info = thread_info.get(&meta.thread_id);
            let created_at = info
                .and_then(|value| value.created_at.clone())
                .or_else(|| meta.started.clone());
            let updated_at = info
                .and_then(|value| value.updated_at.clone())
                .or_else(|| meta.started.clone());
            sessions.push(SessionSummary {
                source: "codex".to_string(),
                session_id: meta.thread_id.clone(),
                project: meta.cwd.clone(),
                project_path: self
                    .host
                    .to_readable(&meta.cwd)
                    .to_string_lossy()
                    .to_string(),
                first_prompt: None,
                agent_title: info
                    .and_then(|value| value.title.clone())
                    .or_else(|| memory_titles.get(&meta.thread_id).cloned()),
                created_at,
                timestamp: updated_at.clone(),
                updated_at,
                file_size_bytes: group.file_size(),
                subagent_count: group.descendants.len().min(u32::MAX as usize) as u32,
                archive_name: None,
                model_contexts: group.model_contexts(),
            });
        }
        sessions.sort_by(|a, b| {
            b.timestamp
                .as_deref()
                .unwrap_or("")
                .cmp(a.timestamp.as_deref().unwrap_or(""))
        });
        Ok(sessions)
    }

    fn index_documents(&self) -> IndexBatch {
        let mut docs = Vec::new();
        let mut failed = 0;
        let memory_titles = self.memory_titles();
        for (sessions_dir, archive_name) in self.rollout_sources() {
            let thread_info = self.thread_info(archive_name.as_deref());
            for group in Self::rollout_groups_in(&sessions_dir) {
                let (doc, group_failed) = self.index_group(&group, archive_name.clone());
                failed += group_failed;
                if let Some(mut doc) = doc {
                    let session_id = doc.session_id.clone();
                    let info = thread_info.get(&session_id);
                    Self::apply_thread_info(&mut doc, info);
                    if doc.agent_title.is_none() {
                        doc.agent_title = memory_titles.get(&session_id).cloned();
                    }
                    doc.version.push_str(&Self::thread_version(info));
                    if let Some(title) = memory_titles.get(&session_id) {
                        doc.version.push_str("|memory-title:");
                        doc.version.push_str(title);
                    }
                    docs.push(doc);
                }
            }
        }
        IndexBatch { docs, failed }
    }

    fn index_manifest(&self) -> Vec<IndexManifestEntry> {
        let mut entries = Vec::new();
        let memory_titles = self.memory_titles();
        for (sessions_dir, archive_name) in self.rollout_sources() {
            let thread_info = self.thread_info(archive_name.as_deref());
            for group in Self::rollout_groups_fast_in(&sessions_dir) {
                let mut version = group.composite_version();
                version.push_str(&Self::thread_version(
                    thread_info.get(&group.root.meta.thread_id),
                ));
                if let Some(title) = memory_titles.get(&group.root.meta.thread_id) {
                    version.push_str("|memory-title:");
                    version.push_str(title);
                }
                entries.push(IndexManifestEntry {
                    key: group.root.path.to_string_lossy().to_string(),
                    version,
                });
            }
        }
        entries
    }

    fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
        let mut docs = Vec::new();
        let mut failed = 0;
        let memory_titles = self.memory_titles();
        for (sessions_dir, archive_name) in self.rollout_sources() {
            let thread_info = self.thread_info(archive_name.as_deref());
            for group in Self::rollout_groups_in(&sessions_dir) {
                if !only.contains(&group.root.path.to_string_lossy().to_string()) {
                    continue;
                }
                let (doc, group_failed) = self.index_group(&group, archive_name.clone());
                failed += group_failed;
                if let Some(mut doc) = doc {
                    let session_id = doc.session_id.clone();
                    let info = thread_info.get(&session_id);
                    Self::apply_thread_info(&mut doc, info);
                    if doc.agent_title.is_none() {
                        doc.agent_title = memory_titles.get(&session_id).cloned();
                    }
                    doc.version.push_str(&Self::thread_version(info));
                    if let Some(title) = memory_titles.get(&session_id) {
                        doc.version.push_str("|memory-title:");
                        doc.version.push_str(title);
                    }
                    docs.push(doc);
                }
            }
        }
        IndexBatch { docs, failed }
    }

    fn project_instruction_candidates(&self, project_path: &Path) -> Vec<InstructionCandidate> {
        vec![
            InstructionCandidate {
                title: "项目 AGENTS.md".to_string(),
                scope: "project",
                kind: "instructions",
                path: project_path.join("AGENTS.md"),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "Codex CLI 仓库指令文件。".to_string(),
            },
            InstructionCandidate {
                title: "项目 .codex/config.toml".to_string(),
                scope: "project",
                kind: "config",
                path: project_path.join(".codex").join("config.toml"),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "Codex CLI 项目配置。".to_string(),
            },
        ]
    }

    fn instruction_project_roots(&self) -> Vec<PathBuf> {
        // The default trait implementation calls list_sessions(), which scans every rollout to
        // collect model contexts. Instruction discovery only needs cwd, which lives in the first
        // session_meta record, so use the fast metadata reader and never traverse gigabytes of
        // conversation content just to look for AGENTS.md.
        self.live_project_inventory().roots
    }

    fn list_rules(&self) -> Result<Vec<RuleFile>, AppError> {
        let mut rules = Vec::new();
        for project_path in self.instruction_project_roots() {
            let path = project_path.join("AGENTS.md");
            if !path.exists() {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            let size_bytes = fs::metadata(&path)?.len();
            rules.push(RuleFile {
                source: self.id().to_string(),
                source_display_name: self.display_name().to_string(),
                scope: "project".to_string(),
                category: project_path.to_string_lossy().to_string(),
                filename: "AGENTS.md".to_string(),
                path: path.to_string_lossy().to_string(),
                content,
                size_bytes,
                enabled: true,
                toggleable: false,
                frontmatter: None,
            });
        }
        rules.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then(a.filename.cmp(&b.filename))
        });
        Ok(rules)
    }

    fn list_memory_projects(&self) -> Result<Vec<ProjectInfo>, AppError> {
        if !self.memories_db.exists() {
            return Ok(Vec::new());
        }
        Ok(vec![ProjectInfo {
            source: self.id().to_string(),
            source_display_name: self.display_name().to_string(),
            slug: "codex-memory".to_string(),
            display_path: "Codex 线程记忆".to_string(),
            memory_count: self.codex_memory_count(),
            session_count: self.live_session_count_fast(),
            last_active: None,
        }])
    }

    fn list_memories(&self, _project: &str) -> Result<Vec<MemoryFile>, AppError> {
        if !self.memories_db.exists() {
            return Ok(Vec::new());
        }
        let conn = self.memory_conn()?;
        // stage1_outputs is keyed by Codex thread_id. For a top-level conversation this is the
        // same stable id used by the Sessions page, so enrich the opaque key with state_5's
        // user-facing thread title. Old/pruned threads fall back to their stored rollout summary.
        let thread_info = self.thread_info(None);
        let mut stmt = conn
            .prepare(
                "SELECT thread_id, raw_memory, rollout_summary, usage_count, last_usage, generated_at \
                 FROM stage1_outputs ORDER BY COALESCE(last_usage, generated_at) DESC",
            )
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let thread_id: String = row.get(0)?;
                let raw_memory: String = row.get(1)?;
                let rollout_summary: String = row.get(2)?;
                let usage_count: Option<i64> = row.get(3)?;
                let title = thread_info
                    .get(&thread_id)
                    .and_then(|info| info.title.clone())
                    .unwrap_or_else(|| {
                        codex_memory_fallback_title(&thread_id, &rollout_summary, &raw_memory)
                    });
                let short_id: String = thread_id.chars().take(8).collect();
                let body = if rollout_summary.trim().is_empty() {
                    raw_memory
                } else {
                    format!("{}\n\n---\n\n{}", raw_memory, rollout_summary)
                };
                Ok(MemoryFile {
                    source: self.id().to_string(),
                    source_display_name: self.display_name().to_string(),
                    project: "codex-memory".to_string(),
                    project_path: "Codex 线程记忆".to_string(),
                    filename: format!("{}.md", thread_id),
                    frontmatter: Some(MemoryFrontmatter {
                        name: Some(title),
                        description: Some(match usage_count {
                            Some(count) => format!("使用 {} 次 · 会话 ID {}", count, short_id),
                            None => format!("会话 ID {}", short_id),
                        }),
                        memory_type: Some("thread".to_string()),
                        metadata: None,
                    }),
                    size_bytes: body.len() as u64,
                    content: body,
                })
            })
            .map_err(|e| AppError::Archive(e.to_string()))?;
        Ok(rows.flatten().collect())
    }

    fn get_memory(&self, project: &str, filename: &str) -> Result<MemoryFile, AppError> {
        self.list_memories(project)?
            .into_iter()
            .find(|memory| memory.filename == filename)
            .ok_or_else(|| AppError::NotFound(format!("{}/{}", project, filename)))
    }

    fn resume_command(&self, session_id: &str, extra_args: &[String]) -> Option<String> {
        Some(
            format!(
                "codex resume {} {}",
                quote_command_arg(session_id),
                extra_args.join(" ")
            )
            .trim()
            .to_string(),
        )
    }

    fn first_prompt(
        &self,
        _project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Option<String> {
        let path = self.session_file(session_id, archive)?;
        let file = File::open(&path).ok()?;
        let reader = BufReader::with_capacity(64 * 1024, file);
        for line in reader.lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
                continue;
            }
            let pl = v.get("payload");
            if pl.and_then(|p| p.get("type")).and_then(|t| t.as_str()) != Some("message") {
                continue;
            }
            if pl.and_then(|p| p.get("role")).and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            let text = join_text(pl.and_then(|p| p.get("content")));
            let trimmed = text.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('<') {
                continue; // injected env/permissions context, not a real prompt
            }
            return Some(text.chars().take(200).collect());
        }
        None
    }

    fn session_detail(
        &self,
        _project: &str,
        session_id: &str,
        byte_offset: u64,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self.session_file(session_id, archive).ok_or_else(|| {
            AppError::NotFound(format!("Codex session not found: {}", session_id))
        })?;
        jsonl::read_seekable_cached(
            &path,
            byte_offset,
            limit,
            min_level,
            &mut CodexParser::new(),
        )
    }

    fn session_tail(
        &self,
        _project: &str,
        session_id: &str,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self.session_file(session_id, archive).ok_or_else(|| {
            AppError::NotFound(format!("Codex session not found: {}", session_id))
        })?;
        jsonl::read_tail_with(&path, limit, min_level, &mut CodexParser::new())
    }

    fn session_before(
        &self,
        _project: &str,
        session_id: &str,
        before_offset: u64,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self.session_file(session_id, archive).ok_or_else(|| {
            AppError::NotFound(format!("Codex session not found: {}", session_id))
        })?;
        jsonl::read_before_with(
            &path,
            before_offset,
            limit,
            min_level,
            &mut CodexParser::new(),
        )
    }

    fn list_subagents(
        &self,
        _project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SubagentInfo>, AppError> {
        let Some(group) = self
            .groups_for_archive(archive)
            .into_iter()
            .find(|group| group.root.meta.thread_id == session_id)
        else {
            return Err(AppError::NotFound(format!(
                "Codex session not found: {}",
                session_id
            )));
        };
        let links = Self::subagent_links(&group);
        let mut out = Vec::with_capacity(group.descendants.len());
        for child in &group.descendants {
            let (tool_use_id, args) = links
                .get(&child.meta.thread_id)
                .cloned()
                .unwrap_or_default();
            let agent_type = child
                .meta
                .agent_role
                .clone()
                .or_else(|| {
                    args.as_ref()
                        .and_then(|a| a.get("agent_type"))
                        .and_then(|x| x.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| "subagent".to_string());
            let description = args
                .as_ref()
                .and_then(|a| a.get("task_name"))
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| child.meta.agent_path.clone())
                .or_else(|| child.meta.agent_nickname.clone())
                .or_else(|| {
                    args.as_ref()
                        .and_then(|a| a.get("message"))
                        .and_then(|x| x.as_str())
                        .map(|message| message.chars().take(160).collect())
                })
                .unwrap_or_else(|| child.meta.thread_id.clone());
            out.push(SubagentInfo {
                agent_id: child.meta.thread_id.clone(),
                agent_type,
                description,
                tool_use_id,
                // The current UI loads until EOF and does not display this estimate. Avoid a
                // second full pass over every forked rollout just to count raw JSONL lines.
                record_count: 0,
            });
        }
        Ok(out)
    }

    fn subagent_detail(
        &self,
        _project: &str,
        session_id: &str,
        agent_id: &str,
        byte_offset: u64,
        limit: u32,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self
            .validated_subagent_path(session_id, agent_id, archive)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Codex subagent {} is not part of session {}",
                    agent_id, session_id
                ))
            })?;
        jsonl::read_seekable_cached(&path, byte_offset, limit, "tool", &mut CodexParser::new())
    }

    fn search_in_session(
        &self,
        _project: &str,
        session_id: &str,
        query: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let Some(path) = self.session_file(session_id, archive) else {
            return Err(AppError::NotFound("Codex session not found".into()));
        };
        let file = File::open(&path)?;
        let mut reader = BufReader::with_capacity(128 * 1024, file);
        let q = query.to_lowercase();
        let mut hits = Vec::new();
        let mut byte_pos: u64 = 0;
        loop {
            let mut line = String::new();
            let n = match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let offset = byte_pos;
            byte_pos += n as u64;
            if !line.to_lowercase().contains(&q) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let pl = v.get("payload");
            let pt = pl
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            // Cover the same scopes as global search: conversation, tool I/O, and reasoning.
            let (record_type, text): (&str, String) = match (t, pt) {
                ("response_item", "message") => (
                    pl.and_then(|p| p.get("role"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("message"),
                    join_text(pl.and_then(|p| p.get("content"))),
                ),
                ("response_item", "function_call") => (
                    "tool",
                    pl.and_then(|p| p.get("arguments"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                ("response_item", "custom_tool_call") => (
                    "tool",
                    pl.and_then(|p| p.get("input"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                ("response_item", "function_call_output")
                | ("response_item", "custom_tool_call_output") => (
                    "tool_result",
                    match pl.and_then(|p| p.get("output")) {
                        Some(Value::String(s)) => s.clone(),
                        Some(o) => o.to_string(),
                        None => String::new(),
                    },
                ),
                ("event_msg", "agent_reasoning") => (
                    "thinking",
                    pl.and_then(|p| p.get("text"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                _ => continue,
            };
            if text.is_empty() || !text.to_lowercase().contains(&q) {
                continue;
            }
            hits.push(SessionSearchHit {
                byte_offset: offset,
                snippet: crate::services::search::extract_snippet(&text, &q),
                record_type: record_type.to_string(),
                timestamp: v.get("timestamp").and_then(|t| t.as_str()).map(fmt_ts),
            });
        }
        Ok(hits)
    }
}

/// Append a kind-tagged chunk to the bounded fast-index collector. Deep search scans the original
/// rollout when exhaustive recall is requested.
fn push_text(texts: &mut FastIndexTextCollector, kind: &str, text: String) {
    texts.push(kind, text);
}

/// Join an array of `{type, text}` content blocks (input_text / output_text / summary_text).
fn join_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ---------- Parser ----------

#[derive(Default)]
pub struct CodexParser {
    /// call_id → [(question id, question text)] for request_user_input, so the matching output
    /// (whose answers are keyed by question id) can be remapped to AskUserQuestion's text-keyed shape.
    ask: HashMap<String, Vec<(String, String)>>,
}

impl CodexParser {
    pub fn new() -> Self {
        CodexParser::default()
    }
}

/// Levels: content = chat (user/assistant/reasoning); tool = +calls/results; debug = +meta/turn/usage.
fn keep(level: &str, min_level: &str) -> bool {
    match min_level {
        "content" => level == "content",
        "tool" => level != "debug",
        _ => true,
    }
}

fn record(record_type: &str, content: String, level: &str, ts: &Option<String>) -> SessionRecord {
    SessionRecord {
        record_type: record_type.to_string(),
        content_preview: content,
        timestamp: ts.clone(),
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        diff: None,
        level: level.to_string(),
        byte_offset: 0,
        group_id: None,
        result_meta: None,
    }
}

impl LineParser for CodexParser {
    fn reset(&mut self) {
        *self = CodexParser::new();
    }

    fn push(&mut self, val: &Value, min_level: &str) -> Vec<SessionRecord> {
        let ts = val.get("timestamp").and_then(|t| t.as_str()).map(fmt_ts);
        let t = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let pl = val.get("payload").cloned().unwrap_or(Value::Null);
        let pt = pl.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let mut out = Vec::new();

        let mut emit = |rec: SessionRecord| {
            if keep(&rec.level, min_level) {
                out.push(rec);
            }
        };

        match t {
            "session_meta" => {
                let cwd = pl.get("cwd").and_then(|x| x.as_str()).unwrap_or("");
                let model = pl
                    .get("model_provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let cli = pl.get("cli_version").and_then(|x| x.as_str()).unwrap_or("");
                emit(record(
                    "meta",
                    format!(
                        "ⓘ codex 会话 · cwd={} · provider={} · cli {}",
                        cwd, model, cli
                    ),
                    "debug",
                    &ts,
                ));
            }
            "compacted" => {
                // Context compaction (like Claude's compact boundary) — show in the conversation.
                let n = pl
                    .get("replacement_history")
                    .and_then(|h| h.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                emit(record(
                    "meta",
                    format!("🗜 上下文已压缩（精简为 {} 条历史）", n),
                    "content",
                    &ts,
                ));
            }
            "turn_context" => {
                let model = pl.get("model").and_then(|x| x.as_str()).unwrap_or("");
                let effort = pl
                    .get("effort")
                    .and_then(|e| {
                        e.as_str()
                            .map(String::from)
                            .or_else(|| e.get("effort").and_then(|x| x.as_str()).map(String::from))
                    })
                    .unwrap_or_default();
                let sandbox = pl
                    .get("sandbox_policy")
                    .and_then(|s| s.get("type"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                emit(record(
                    "turn",
                    format!("⚙ 轮次 · model={} · effort={} · {}", model, effort, sandbox),
                    "debug",
                    &ts,
                ));
            }
            "event_msg" => {
                // Mostly duplicates of response_item (dropped). We keep: usage, errors, the READABLE
                // reasoning (response_item.reasoning is ~95% encrypted), and unique state events.
                match pt {
                    "token_count" => {
                        let info = pl.get("info");
                        let last = info.and_then(|i| i.get("last_token_usage"));
                        let total = info.and_then(|i| i.get("total_token_usage"));
                        let g = |o: Option<&Value>, k: &str| {
                            o.and_then(|u| u.get(k))
                                .and_then(|x| x.as_i64())
                                .unwrap_or(0)
                        };
                        let ctx = info
                            .and_then(|i| i.get("model_context_window"))
                            .and_then(|x| x.as_i64())
                            .unwrap_or(0);
                        // Codex reports only cached_input_tokens (cache READ/hit) — no separate write.
                        emit(record(
                            "usage",
                            format!(
                                "📊 tokens · 输入 {} (缓存读 {}) · 输出 {} (思考 {}) · 累计 {}/{}",
                                g(last, "input_tokens"),
                                g(last, "cached_input_tokens"),
                                g(last, "output_tokens"),
                                g(last, "reasoning_output_tokens"),
                                g(total, "total_tokens"),
                                ctx
                            ),
                            "debug",
                            &ts,
                        ));
                    }
                    "error" => {
                        let msg = pl.get("message").and_then(|x| x.as_str()).unwrap_or("");
                        emit(record("meta", format!("⚠ error: {}", msg), "debug", &ts));
                    }
                    // The actual readable chain-of-thought (response_item.reasoning is encrypted).
                    "agent_reasoning" => {
                        let text = pl.get("text").and_then(|x| x.as_str()).unwrap_or("");
                        if !text.trim().is_empty() {
                            emit(record("thinking", text.to_string(), "content", &ts));
                        }
                    }
                    // Unique conversation-flow events with no response_item equivalent.
                    "turn_aborted" => {
                        let reason = pl
                            .get("reason")
                            .and_then(|x| x.as_str())
                            .unwrap_or("aborted");
                        emit(record(
                            "meta",
                            format!("⛔ 该轮被中断（{}）", reason),
                            "content",
                            &ts,
                        ));
                    }
                    "thread_rolled_back" => {
                        let n = pl.get("num_turns").and_then(|x| x.as_i64()).unwrap_or(0);
                        emit(record(
                            "meta",
                            format!("↩ 对话回滚 {} 轮", n),
                            "content",
                            &ts,
                        ));
                    }
                    "task_started" => {
                        emit(record("meta", "▶ 轮次开始".to_string(), "debug", &ts));
                    }
                    "task_complete" => {
                        let mut s = "■ 轮次结束".to_string();
                        if let Some(d) = pl.get("duration_ms").and_then(|x| x.as_i64()) {
                            s.push_str(&format!(" · 耗时 {:.1}s", d as f64 / 1000.0));
                        }
                        if let Some(t) = pl.get("time_to_first_token_ms").and_then(|x| x.as_i64()) {
                            s.push_str(&format!(" · 首字 {}ms", t));
                        }
                        emit(record("meta", s, "debug", &ts));
                    }
                    // context_compacted is the empty signal; the richer top-level `compacted` carries it.
                    _ => {}
                }
            }
            "response_item" => match pt {
                "message" => {
                    let role = pl.get("role").and_then(|x| x.as_str()).unwrap_or("");
                    let text = join_text(pl.get("content"));
                    match role {
                        "assistant" => emit(record("assistant", text, "content", &ts)),
                        "developer" => emit(record(
                            "meta",
                            format!("ⓘ developer: {}", text),
                            "debug",
                            &ts,
                        )),
                        "user" => {
                            let injected = text.trim_start().starts_with('<');
                            if injected {
                                emit(record("meta", format!("ⓘ {}", text), "debug", &ts));
                            } else {
                                emit(record("user", text, "content", &ts));
                            }
                        }
                        _ => {}
                    }
                }
                // Rendered via event_msg.agent_reasoning (readable text); response_item.reasoning is
                // API-internal and ~95% encrypted, so emitting from it would mostly be empty/dup.
                "reasoning" => {}
                "function_call" => {
                    let name = pl.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                    let call_id = pl.get("call_id").and_then(|x| x.as_str());
                    let args = pl
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|s| serde_json::from_str::<Value>(s).ok());
                    let is_ask = matches!(name, "request_user_input" | "ask_user_question");
                    if is_ask {
                        if let (Some(cid), Some(a)) = (call_id, args.as_ref()) {
                            let qs: Vec<(String, String)> = a
                                .get("questions")
                                .and_then(|q| q.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|q| {
                                            let id =
                                                q.get("id").and_then(|x| x.as_str())?.to_string();
                                            let text = q
                                                .get("question")
                                                .and_then(|x| x.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                            Some((id, text))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            self.ask.insert(cid.to_string(), qs);
                        }
                    }
                    // request_user_input questions match AskUserQuestion's shape directly; render it
                    // as the interactive card (content level, part of the conversation).
                    let level = if is_ask { "content" } else { "tool" };
                    let mut r = record("assistant", String::new(), level, &ts);
                    r.tool_name = Some(if is_ask {
                        "ask_user_question".to_string()
                    } else {
                        name.to_string()
                    });
                    r.tool_use_id = call_id.map(String::from);
                    r.tool_input = args;
                    emit(r);
                }
                "custom_tool_call" => {
                    let name = pl.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                    let call_id = pl.get("call_id").and_then(|x| x.as_str());
                    let input = pl.get("input").and_then(|x| x.as_str()).unwrap_or("");
                    let mut r = record("assistant", String::new(), "tool", &ts);
                    r.tool_name = Some(name.to_string());
                    r.tool_use_id = call_id.map(String::from);
                    r.tool_input = Some(serde_json::json!({ "input": input }));
                    emit(r);
                }
                "web_search_call" => {
                    let call_id = pl.get("call_id").and_then(|x| x.as_str());
                    let mut r = record("assistant", String::new(), "tool", &ts);
                    r.tool_name = Some("web_search".to_string());
                    r.tool_use_id = call_id.map(String::from);
                    r.tool_input = pl.get("action").cloned();
                    emit(r);
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let call_id = pl.get("call_id").and_then(|x| x.as_str());
                    let raw = match pl.get("output") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    if let Some(qs) = call_id.and_then(|c| self.ask.remove(c)) {
                        // request_user_input answer → AskUserQuestion result_meta (text-keyed).
                        let mut r = record("tool_result", String::new(), "content", &ts);
                        r.tool_use_id = call_id.map(String::from);
                        r.result_meta = build_ask_answers(&raw, &qs);
                        emit(r);
                    } else {
                        // Shell results carry "Exit code: N\nWall time: X seconds\nOutput:\n<body>".
                        // Lift exit code / duration into structured meta and strip the header.
                        let (content, meta) = parse_shell_output(&raw);
                        let mut r = record("tool_result", content, "tool", &ts);
                        r.tool_use_id = call_id.map(String::from);
                        r.result_meta = meta;
                        emit(r);
                    }
                }
                _ => {}
            },
            _ => {}
        }

        out
    }

    fn flush(&mut self, _min_level: &str) -> Vec<SessionRecord> {
        Vec::new() // Codex messages are one record per line — nothing accumulates across lines.
    }

    fn group_of(&self, _val: &Value) -> Option<String> {
        None // Codex calls render sequentially; results join by call_id in the frontend.
    }

    fn skippable(&self, _line: &str, _min_level: &str) -> bool {
        false // Codex rollouts are small; always parse.
    }
}

/// Build AskUserQuestion `result_meta` from a request_user_input output. The output is a JSON
/// string `{"answers":{"<qid>":{"answers":[label,…]}}}`; we remap it to `{question_text: [labels]}`.
/// Returns None if the output isn't answer JSON (e.g. "…unavailable in Default mode").
fn build_ask_answers(raw: &str, qs: &[(String, String)]) -> Option<Value> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let ans = parsed.get("answers")?.as_object()?;
    let mut map = serde_json::Map::new();
    for (id, text) in qs {
        if let Some(entry) = ans.get(id) {
            let labels = entry
                .get("answers")
                .cloned()
                .or_else(|| entry.is_array().then(|| entry.clone()))
                .unwrap_or_else(|| Value::Array(vec![]));
            map.insert(text.clone(), labels);
        }
    }
    if map.is_empty() {
        None
    } else {
        Some(serde_json::json!({ "answers": map }))
    }
}

/// Split a Codex shell `function_call_output` into (clean_output, terminal_meta).
/// Header shape: `Exit code: N` / `Wall time: X seconds` / `Output:` / <body>. Anything that
/// doesn't match (e.g. "Plan updated", apply_patch results) passes through unchanged with no meta.
fn parse_shell_output(raw: &str) -> (String, Option<Value>) {
    if !raw.starts_with("Exit code:") {
        return (raw.to_string(), None);
    }
    let norm = raw.replace("\r\n", "\n");
    let mut parts = norm.splitn(4, '\n');
    let exit_line = parts.next().unwrap_or("");
    let wall_line = parts.next().unwrap_or("");
    let out_marker = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();
    if !out_marker.trim_start().starts_with("Output:") {
        return (raw.to_string(), None);
    }
    let exit_code: Option<i64> = exit_line
        .trim_start_matches("Exit code:")
        .trim()
        .parse()
        .ok();
    let duration_ms: Option<i64> = wall_line
        .trim_start_matches("Wall time:")
        .trim()
        .trim_end_matches("seconds")
        .trim()
        .parse::<f64>()
        .ok()
        .map(|s| (s * 1000.0) as i64);
    let meta = serde_json::json!({
        "terminal": { "exit_code": exit_code, "duration_ms": duration_ms }
    });
    (body, Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempRolloutDir {
        path: PathBuf,
    }

    impl TempRolloutDir {
        fn new() -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "code-dejavu-codex-{}-{}",
                std::process::id(),
                id
            ));
            fs::create_dir_all(&path).expect("create rollout dir");
            Self { path }
        }

        fn write(&self, id: &str, records: &[Value]) -> PathBuf {
            let path = self.path.join(format!("rollout-test-{id}.jsonl"));
            let mut body = records
                .iter()
                .map(|record| serde_json::to_string(record).expect("serialize rollout record"))
                .collect::<Vec<_>>()
                .join("\n");
            body.push('\n');
            fs::write(&path, body).expect("write rollout");
            path
        }

        fn provider(&self) -> CodexProvider {
            CodexProvider {
                codex_dir: self.path.clone(),
                sessions_dir: self.path.clone(),
                memories_db: self.path.join("missing-memories.sqlite"),
                archive_root: self.path.join("missing-archives"),
                host: Host::Native,
                project_inventory_cache: Mutex::new(None),
            }
        }
    }

    impl Drop for TempRolloutDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn session_meta(payload: Value) -> Value {
        json!({
            "timestamp": "2026-07-11T00:00:00Z",
            "type": "session_meta",
            "payload": payload
        })
    }

    fn root_meta(id: &str) -> Value {
        session_meta(json!({
            "id": id,
            "session_id": id,
            "cwd": "C:/repo",
            "thread_source": "user",
            "source": "cli"
        }))
    }

    #[test]
    fn instruction_file_preserves_existing_case_and_defaults_to_lowercase() {
        let dir = TempRolloutDir::new();
        assert_eq!(
            CodexProvider::instruction_file_in(&dir.path),
            dir.path.join("instruction.md")
        );
        let capitalized = dir.path.join("Instruction.md");
        fs::write(&capitalized, "instructions").expect("write instruction file");
        assert_eq!(CodexProvider::instruction_file_in(&dir.path), capitalized);
    }

    #[test]
    fn memory_title_prefers_a_readable_rollout_summary() {
        assert_eq!(
            codex_memory_fallback_title(
                "019fa680-c48f",
                "\n## Improve session management\nMore detail",
                "raw"
            ),
            "Improve session management"
        );
        assert_eq!(
            codex_memory_fallback_title("019fa680-c48f", "", ""),
            "Codex 会话 019fa680"
        );
        assert!(
            readable_session_title("???????? read ?? packages/core/src/ir.py ???".to_string())
                .is_none()
        );
        assert_eq!(
            readable_session_title("Why does this fail?".to_string()).as_deref(),
            Some("Why does this fail?")
        );
    }

    fn child_meta(id: &str, root: &str, parent: &str, depth: u32) -> Value {
        session_meta(json!({
            "id": id,
            "session_id": root,
            "parent_thread_id": parent,
            "cwd": "C:/repo",
            "thread_source": "subagent",
            "agent_path": format!("/root/{id}"),
            "source": {"subagent": {"thread_spawn": {
                "parent_thread_id": parent,
                "depth": depth,
                "agent_path": format!("/root/{id}"),
                "agent_nickname": "Ada",
                "agent_role": "worker"
            }}}
        }))
    }

    fn token_count(input: u64, output: u64, cache: u64, total: u64) -> Value {
        json!({"type":"event_msg","payload":{
            "type":"token_count","info":{"total_token_usage":{
                "input_tokens":input,"output_tokens":output,
                "cached_input_tokens":cache,"total_tokens":total
            }}
        }})
    }

    #[test]
    fn parse_shell_output_lifts_exit_and_duration() {
        let raw = "Exit code: 0\nWall time: 1.5 seconds\nOutput:\nhello\nworld";
        let (body, meta) = parse_shell_output(raw);
        assert_eq!(body, "hello\nworld");
        let meta = meta.expect("meta");
        assert_eq!(meta["terminal"]["exit_code"], json!(0));
        assert_eq!(meta["terminal"]["duration_ms"], json!(1500));
    }

    #[test]
    fn rollout_identity_uses_only_the_first_session_meta() {
        let dir = TempRolloutDir::new();
        let child = child_meta("child", "root", "root", 1);
        let path = dir.write(
            "child",
            &[
                child,
                root_meta("root"),
                json!({"type":"turn_context","payload":{"model":"gpt-5","effort":"high"}}),
            ],
        );

        let meta = CodexProvider::read_meta(&path).expect("rollout meta");
        assert_eq!(meta.thread_id, "child");
        assert_eq!(meta.logical_session_id.as_deref(), Some("root"));
        assert_eq!(meta.parent_thread_id.as_deref(), Some("root"));
        assert!(meta.is_subagent);
        assert_eq!(meta.agent_path.as_deref(), Some("/root/child"));
        assert_eq!(meta.model_contexts[0].model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn fast_metadata_reader_has_a_strict_scan_budget() {
        let dir = TempRolloutDir::new();
        let path = dir.path.join("rollout-delayed-meta.jsonl");
        let delayed_meta = serde_json::to_string(&root_meta("late")).expect("serialize meta");
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                "x".repeat(FAST_META_SCAN_BYTES as usize),
                delayed_meta
            ),
        )
        .expect("write delayed metadata rollout");

        assert!(CodexProvider::read_meta_fast(&path).is_none());
        assert_eq!(
            CodexProvider::read_meta(&path)
                .expect("full metadata reader")
                .thread_id,
            "late"
        );
    }

    #[test]
    fn instruction_project_roots_uses_first_line_session_metadata() {
        let dir = TempRolloutDir::new();
        let first_root = dir.path.join("project-a");
        let second_root = dir.path.join("project-b");
        dir.write(
            "a",
            &[session_meta(json!({
                "id": "a",
                "session_id": "a",
                "cwd": first_root,
                "thread_source": "user",
                "source": "cli"
            }))],
        );
        dir.write(
            "b",
            &[session_meta(json!({
                "id": "b",
                "session_id": "b",
                "cwd": second_root,
                "thread_source": "user",
                "source": "cli"
            }))],
        );
        // A duplicate cwd must not make instruction discovery scan the same project twice.
        dir.write(
            "duplicate",
            &[session_meta(json!({
                "id": "duplicate",
                "session_id": "duplicate",
                "cwd": first_root,
                "thread_source": "user",
                "source": "cli"
            }))],
        );

        let provider = dir.provider();
        let mut expected = vec![first_root, second_root];
        expected.sort();
        assert_eq!(provider.instruction_project_roots(), expected);
    }

    #[test]
    fn fast_session_count_groups_subagents_without_scanning_contexts() {
        let dir = TempRolloutDir::new();
        dir.write(
            "root",
            &[
                root_meta("root"),
                json!({"type":"turn_context","payload":{"model":"must-not-be-read"}}),
            ],
        );
        dir.write("child", &[child_meta("child", "root", "root", 1)]);
        dir.write(
            "other",
            &[session_meta(json!({
                "id": "other",
                "session_id": "other",
                "cwd": "C:/other",
                "thread_source": "user",
                "source": "cli"
            }))],
        );

        assert_eq!(dir.provider().live_session_count_fast(), 2);
    }

    #[test]
    fn old_review_source_is_recognized_as_a_subagent() {
        let dir = TempRolloutDir::new();
        let path = dir.write(
            "review",
            &[session_meta(json!({
                "id": "review",
                "cwd": "C:/repo",
                "parent_thread_id": "root",
                "source": {"subagent": "review"}
            }))],
        );

        let meta = CodexProvider::read_meta(&path).expect("old review meta");
        assert!(meta.is_subagent);
        assert_eq!(meta.parent_thread_id.as_deref(), Some("root"));
    }

    #[test]
    fn legacy_subagent_without_assignment_keeps_independent_usage() {
        let dir = TempRolloutDir::new();
        let path = dir.write(
            "review",
            &[
                session_meta(json!({
                    "id": "review", "cwd": "C:/repo",
                    "parent_thread_id": "root", "source": {"subagent": "review"}
                })),
                token_count(80, 20, 10, 100),
            ],
        );
        let provider = dir.provider();
        let (doc, failed) = provider.index_one(&path, None);
        assert!(!failed);
        assert_eq!(doc.expect("index doc").tokens.total_tokens, 100);
    }

    #[test]
    fn groups_nested_subagents_under_one_root() {
        let dir = TempRolloutDir::new();
        dir.write("root", &[root_meta("root")]);
        dir.write(
            "child",
            &[
                child_meta("child", "root", "root", 1),
                token_count(100, 20, 10, 120),
                json!({"type":"response_item","payload":{
                    "type":"agent_message","author":"/root",
                    "recipient":"/root/other","content":"inherited coordination"
                }}),
                token_count(110, 25, 12, 135),
                json!({"type":"response_item","payload":{
                    "type":"agent_message","author":"/root",
                    "recipient":"/root/child","content":"assignment"
                }}),
                token_count(150, 35, 20, 185),
                json!({"type":"response_item","payload":{
                    "type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":"child-only-marker"}]
                }}),
            ],
        );
        dir.write(
            "grandchild",
            &[child_meta("grandchild", "root", "child", 2)],
        );

        let groups = CodexProvider::rollout_groups_in(&dir.path);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root.meta.thread_id, "root");
        assert_eq!(groups[0].descendants.len(), 2);
        assert!(groups[0]
            .descendants
            .iter()
            .any(|entry| entry.meta.thread_id == "grandchild"));

        let provider = dir.provider();
        let sessions = provider.list_sessions(None).expect("session summaries");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "root");
        assert_eq!(sessions[0].subagent_count, 2);
        let batch = provider.index_documents();
        assert_eq!(batch.failed, 0);
        assert_eq!(batch.docs.len(), 1);
        assert_eq!(batch.docs[0].session_id, "root");
        assert_eq!(batch.docs[0].subagent_count, 2);
        assert_eq!(batch.docs[0].tokens.input_tokens, 40);
        assert_eq!(batch.docs[0].tokens.output_tokens, 10);
        assert_eq!(batch.docs[0].tokens.cache_tokens, 8);
        assert_eq!(batch.docs[0].tokens.total_tokens, 50);
        assert!(batch.docs[0]
            .texts
            .iter()
            .any(|text| text.text.contains("child-only-marker")));
        assert!(provider
            .validated_subagent_path("root", "grandchild", None)
            .is_some());
        assert!(provider
            .validated_subagent_path("child", "grandchild", None)
            .is_none());
    }

    #[test]
    fn user_fork_is_not_mistaken_for_a_subagent() {
        let dir = TempRolloutDir::new();
        dir.write("root", &[root_meta("root")]);
        dir.write(
            "fork",
            &[session_meta(json!({
                "id": "fork",
                "session_id": "fork",
                "cwd": "C:/repo",
                "thread_source": "user",
                "source": "cli",
                "forked_from_id": "root"
            }))],
        );

        let mut roots: Vec<String> = CodexProvider::rollout_groups_in(&dir.path)
            .into_iter()
            .map(|group| group.root.meta.thread_id)
            .collect();
        roots.sort();
        assert_eq!(roots, vec!["fork", "root"]);
    }

    #[test]
    fn orphan_parent_is_promoted_and_keeps_its_descendant() {
        let dir = TempRolloutDir::new();
        dir.write("orphan", &[child_meta("orphan", "missing", "missing", 1)]);
        dir.write("nested", &[child_meta("nested", "missing", "orphan", 2)]);

        let groups = CodexProvider::rollout_groups_in(&dir.path);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root.meta.thread_id, "orphan");
        assert_eq!(groups[0].descendants[0].meta.thread_id, "nested");
    }

    #[test]
    fn started_activity_links_spawn_call_to_child_thread() {
        let dir = TempRolloutDir::new();
        dir.write(
            "root",
            &[
                root_meta("root"),
                json!({"type":"response_item","payload":{
                    "type":"function_call","name":"spawn_agent","call_id":"call-1",
                    "arguments":"{\"task_name\":\"child\",\"agent_type\":\"worker\"}"
                }}),
                json!({"type":"event_msg","payload":{
                    "type":"sub_agent_activity","kind":"started","event_id":"call-1",
                    "agent_thread_id":"child","agent_path":"/root/child"
                }}),
            ],
        );
        dir.write("child", &[child_meta("child", "root", "root", 1)]);
        let group = CodexProvider::rollout_groups_in(&dir.path)
            .into_iter()
            .next()
            .expect("group");

        let links = CodexProvider::subagent_links(&group);
        let (call_id, args) = links.get("child").expect("child link");
        assert_eq!(call_id, "call-1");
        assert_eq!(
            args.as_ref()
                .and_then(|a| a.get("task_name"))
                .and_then(|x| x.as_str()),
            Some("child")
        );
    }

    #[test]
    fn older_spawn_output_links_child_without_activity_event() {
        let dir = TempRolloutDir::new();
        dir.write(
            "root",
            &[
                root_meta("root"),
                json!({"type":"response_item","payload":{
                    "type":"function_call","name":"spawn_agent","call_id":"call-old",
                    "arguments":"{\"agent_type\":\"worker\",\"message\":\"do work\"}"
                }}),
                json!({"type":"response_item","payload":{
                    "type":"function_call_output","call_id":"call-old",
                    "output":"{\"agent_id\":\"child\",\"nickname\":\"Ada\"}"
                }}),
            ],
        );
        dir.write("child", &[child_meta("child", "root", "root", 1)]);
        let group = CodexProvider::rollout_groups_in(&dir.path)
            .into_iter()
            .next()
            .expect("group");

        let links = CodexProvider::subagent_links(&group);
        assert_eq!(
            links.get("child").map(|link| link.0.as_str()),
            Some("call-old")
        );
    }

    #[test]
    fn parse_shell_output_passes_through_non_shell() {
        let (body, meta) = parse_shell_output("Plan updated");
        assert_eq!(body, "Plan updated");
        assert!(meta.is_none());
    }

    #[test]
    fn join_text_concats_blocks_and_strings() {
        assert_eq!(join_text(Some(&json!("plain"))), "plain");
        assert_eq!(
            join_text(Some(&json!([
                {"type":"input_text","text":"a"},
                {"type":"output_text","text":"b"}
            ]))),
            "ab"
        );
        assert_eq!(join_text(None), "");
    }

    #[test]
    fn codex_parses_assistant_message() {
        let mut p = CodexParser::new();
        let v = json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}});
        let out = p.push(&v, "content");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record_type, "assistant");
        assert_eq!(out[0].content_preview, "hi");
    }

    #[test]
    fn codex_function_call_is_a_tool_record() {
        let mut p = CodexParser::new();
        let v = json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1","arguments":"{\"command\":\"ls\"}"}});
        let out = p.push(&v, "tool");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool_name.as_deref(), Some("shell"));
        assert_eq!(out[0].tool_use_id.as_deref(), Some("c1"));
    }

    #[test]
    fn codex_agent_reasoning_renders_as_thinking() {
        let mut p = CodexParser::new();
        let v =
            json!({"type":"event_msg","payload":{"type":"agent_reasoning","text":"thinking..."}});
        let out = p.push(&v, "content");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record_type, "thinking");
        assert_eq!(out[0].content_preview, "thinking...");
    }
}
