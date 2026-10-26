//! DeepSeek Harness (`dsh`) provider.
//!
//! DSH stores one append-only session log per session under
//! `$DSH_HOME/sessions/--<project>--/<encoded-session-id>/session[.vN].jsonl[.zstd]`.
//! The header line carries identity/cwd/parent/delegation data; every following line is a
//! `SessionEvent` (`user/message`, `assistant/message`, `tool/call`, `tool/result`, …).
//!
//! The default backend writes checksummed Zstandard frames, optionally mixed with historical raw
//! generations, so this provider reads both encodings and selects the highest format version.

use super::{
    quote_command_arg, AgentProvider, Capabilities, FastIndexTextCollector, IndexBatch, IndexDoc,
    IndexManifestEntry, InstructionCandidate, TokenUsage,
};
use crate::error::AppError;
use crate::hosts::Host;
use crate::models::profile::ProfileArchive;
use crate::models::session::{
    push_model_context, PaginatedRecords, SessionModelInfo, SessionRecord, SessionSearchHit,
    SessionSummary, SubagentInfo,
};
use crate::paths::app_data_dir;
use crate::services::profile_archiver::{self, SnapshotItem, SnapshotSpec};
use chrono::{DateTime, Local};
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn fmt_ms(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            let local: DateTime<Local> = dt.into();
            local.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_default()
}

fn fmt_ms_full(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            let local: DateTime<Local> = dt.into();
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_default()
}

fn clean_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn block_text(content: &Value) -> (String, String) {
    // Returns (visible text, reasoning text), joining every known block kind.
    let mut text = Vec::new();
    let mut reasoning = Vec::new();
    let Some(blocks) = content.as_array() else {
        return (String::new(), String::new());
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    text.push(value.to_string());
                }
            }
            Some("reasoning") => {
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    reasoning.push(value.to_string());
                }
            }
            Some("image") => text.push("[image]".to_string()),
            Some("file") => {
                let name = block
                    .pointer("/attachment/name")
                    .and_then(Value::as_str)
                    .unwrap_or("file");
                text.push(format!("[file: {name}]"));
            }
            _ => {}
        }
    }
    (text.join("\n"), reasoning.join("\n"))
}

fn tool_content(message: &Value) -> String {
    block_text(message.get("content").unwrap_or(&Value::Null)).0
}

fn arguments_value(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!({ "arguments": raw }))
}

#[derive(Clone, Debug)]
struct DshSessionMeta {
    id: String,
    /// Project directory name on disk (`--<normalized cwd>--`); used as the opaque project key.
    project: String,
    project_path: String,
    file: PathBuf,
    created_ms: i64,
    updated_ms: i64,
    size: u64,
    is_subagent: bool,
    is_seeded: bool,
    parent_session: Option<String>,
    agent_preset: Option<String>,
    archive_name: Option<String>,
}

pub struct DshProvider {
    dsh_home: PathBuf,
    sessions_root: PathBuf,
    agents_home: PathBuf,
    archive_root: PathBuf,
    host: Host,
}

impl Default for DshProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DshProvider {
    pub fn new() -> Self {
        let home = home();
        let dsh_home = std::env::var("DSH_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".dsh"));
        let agents_home = std::env::var("DSH_AGENTS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".agents"));
        Self::from_home(Host::Native, dsh_home, agents_home)
    }

    pub fn for_host(host: Host, home: &Path) -> Self {
        Self::from_home(host, home.join(".dsh"), home.join(".agents"))
    }

    fn from_home(host: Host, dsh_home: PathBuf, agents_home: PathBuf) -> Self {
        let archive_root = match host.tag() {
            Some(_) => app_data_dir()
                .join("archives")
                .join("dsh")
                .join(format!("wsl-{}", host.key())),
            None => app_data_dir().join("archives").join("dsh"),
        };
        Self {
            sessions_root: dsh_home.join("sessions"),
            dsh_home,
            agents_home,
            archive_root,
            host,
        }
    }

    fn snapshot_spec(&self) -> SnapshotSpec {
        SnapshotSpec {
            source: "dsh",
            display_name: "DeepSeek Harness",
            archive_root: self.archive_root.clone(),
            items: vec![SnapshotItem {
                name: "dsh",
                path: self.dsh_home.clone(),
                preserve: &["credentials", "credentials.json", "auth.json"],
            }],
            clear_current_on_create: true,
        }
    }

    fn session_sources(&self) -> Vec<(PathBuf, Option<String>)> {
        let mut sources = Vec::new();
        if self.sessions_root.exists() {
            sources.push((self.sessions_root.clone(), None));
        }
        if let Ok(archives) = fs::read_dir(&self.archive_root) {
            for entry in archives.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let archive_name = entry.file_name().to_string_lossy().to_string();
                let sessions = entry.path().join("dsh").join("sessions");
                if sessions.exists() {
                    sources.push((sessions, Some(archive_name)));
                }
            }
        }
        sources
    }

    /// Parse a canonical generation filename: `session.jsonl` is v0, `session.vN.jsonl` is vN.
    fn generation_version(name: &str) -> Option<(u32, bool)> {
        let (base, compressed) = match name.strip_suffix(".zstd") {
            Some(base) => (base, true),
            None => (name, false),
        };
        if base == "session.jsonl" {
            return Some((0, compressed));
        }
        let version = base.strip_prefix("session.v")?.strip_suffix(".jsonl")?;
        if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some((version.parse().ok()?, compressed))
    }

    /// The highest canonical generation, preferring the raw encoding on a version tie so a
    /// read-only consumer never has to decompress when both were published.
    fn generation_file(dir: &Path) -> Option<PathBuf> {
        let mut best: Option<(u32, bool, PathBuf)> = None;
        for entry in fs::read_dir(dir).ok()?.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Some((version, compressed)) = Self::generation_version(&name) else {
                continue;
            };
            let candidate = entry.path();
            let replace = match &best {
                Some((best_version, best_compressed, _)) => {
                    version > *best_version
                        || (version == *best_version && *best_compressed && !compressed)
                }
                None => true,
            };
            if replace {
                best = Some((version, compressed, candidate));
            }
        }
        best.map(|(_, _, path)| path)
    }

    fn reader(path: &Path) -> Result<Box<dyn BufRead>, AppError> {
        let file = File::open(path)?;
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zstd"))
        {
            let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
                .map_err(|e| AppError::Archive(format!("open dsh zstd session: {e}")))?;
            Ok(Box::new(BufReader::new(decoder)))
        } else {
            Ok(Box::new(BufReader::new(file)))
        }
    }

    fn read_header(path: &Path) -> Option<Value> {
        let mut reader = Self::reader(path).ok()?;
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let value = serde_json::from_str::<Value>(&line).ok()?;
        (value.get("type").and_then(Value::as_str) == Some("session")).then_some(value)
    }

    fn load_events(path: &Path) -> Result<Vec<Value>, AppError> {
        let mut reader = Self::reader(path)?;
        let mut events = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) == Some("session") {
                continue;
            }
            events.push(value);
        }
        Ok(events)
    }

    fn scan_root(root: &Path, archive_name: Option<String>, host: &Host) -> Vec<DshSessionMeta> {
        if !root.exists() {
            return Vec::new();
        }
        let mut sessions = Vec::new();
        for project in fs::read_dir(root).into_iter().flatten().flatten() {
            if !project.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let project_key = project.file_name().to_string_lossy().to_string();
            for entry in fs::read_dir(project.path()).into_iter().flatten().flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let Some(file) = Self::generation_file(&entry.path()) else {
                    continue;
                };
                let Some(header) = Self::read_header(&file) else {
                    continue;
                };
                let Some(id) = clean_str(header.get("id")) else {
                    continue;
                };
                let cwd = clean_str(header.get("cwd")).unwrap_or_default();
                let project_path = host.to_readable(&cwd).to_string_lossy().to_string();
                let created_ms = header.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
                let updated_ms = fs::metadata(&file)
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(created_ms);
                let size = fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
                sessions.push(DshSessionMeta {
                    id,
                    project: project_key.clone(),
                    project_path,
                    file,
                    created_ms,
                    updated_ms,
                    size,
                    is_subagent: header.get("origin").and_then(Value::as_str) == Some("subagent"),
                    is_seeded: header
                        .get("isSeeded")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    parent_session: clean_str(header.get("parentSession")),
                    agent_preset: clean_str(header.get("agentPreset")),
                    archive_name: archive_name.clone(),
                });
            }
        }
        sessions
    }

    fn scan_all(&self) -> Vec<DshSessionMeta> {
        let mut sessions = Vec::new();
        for (root, archive) in self.session_sources() {
            sessions.extend(Self::scan_root(&root, archive, &self.host));
        }
        sessions
    }

    fn count_children(sessions: &[DshSessionMeta], id: &str) -> u32 {
        sessions
            .iter()
            .filter(|session| session.parent_session.as_deref() == Some(id))
            .count() as u32
    }

    fn summary_json(session: &DshSessionMeta, sessions: &[DshSessionMeta]) -> SessionSummary {
        SessionSummary {
            source: "dsh".to_string(),
            session_id: session.id.clone(),
            project: session.project.clone(),
            project_path: session.project_path.clone(),
            first_prompt: None,
            agent_title: None,
            created_at: Some(fmt_ms(session.created_ms)),
            updated_at: Some(fmt_ms(session.updated_ms)),
            timestamp: Some(fmt_ms(session.updated_ms)),
            file_size_bytes: session.size,
            subagent_count: Self::count_children(sessions, &session.id),
            archive_name: session.archive_name.clone(),
            model_contexts: Vec::new(),
        }
    }

    fn session_by_id(&self, session_id: &str, archive: Option<&str>) -> Option<DshSessionMeta> {
        self.scan_all().into_iter().find(|session| {
            session.id == session_id
                && session.archive_name.as_deref() == archive.filter(|name| !name.is_empty())
        })
    }

    fn index_for(&self, session: &DshSessionMeta, sessions: &[DshSessionMeta]) -> IndexDoc {
        let (title, first_prompt, texts, models, tokens) = self.parse_index(session);
        IndexDoc {
            source: "dsh".to_string(),
            session_id: session.id.clone(),
            project: session.project.clone(),
            project_path: session.project_path.clone(),
            created_at: Some(fmt_ms(session.created_ms)),
            updated_at: Some(fmt_ms(session.updated_ms)),
            agent_title: title,
            timestamp: Some(fmt_ms(session.updated_ms)),
            file_size_bytes: session.size,
            subagent_count: Self::count_children(sessions, &session.id),
            archive_name: session.archive_name.clone(),
            first_prompt,
            model_contexts: models,
            texts,
            tokens,
            key: session.file.to_string_lossy().to_string(),
            version: format!("{}:{}", session.size, session.updated_ms),
        }
    }

    fn parse_index(
        &self,
        session: &DshSessionMeta,
    ) -> (
        Option<String>,
        Option<String>,
        Vec<super::IndexText>,
        Vec<SessionModelInfo>,
        TokenUsage,
    ) {
        let Ok(events) = Self::load_events(&session.file) else {
            return (None, None, Vec::new(), Vec::new(), TokenUsage::default());
        };
        let mut texts = FastIndexTextCollector::default();
        let mut title = None;
        let mut first_prompt = None;
        let mut models = Vec::new();
        let mut tokens = TokenUsage::default();
        let seeded = session.is_seeded;
        let mut live = !seeded;

        for event in events {
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
            let data = event.get("data").unwrap_or(&Value::Null);
            if event_type == "session/end-seed" {
                live = true;
                continue;
            }
            if seeded && !live {
                continue;
            }
            match event_type {
                "session/title" => {
                    title = clean_str(data.get("title")).or(title);
                }
                "request/context" => {
                    push_model_context(
                        &mut models,
                        clean_str(data.get("provider")),
                        clean_str(data.get("model")),
                        None,
                    );
                }
                "user/message" => {
                    let (text, _) = block_text(data.get("content").unwrap_or(&Value::Null));
                    if text.trim().is_empty() {
                        continue;
                    }
                    let human = data
                        .pointer("/source/kind")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "user");
                    if !human {
                        continue;
                    }
                    if first_prompt.is_none() {
                        first_prompt = Some(text.chars().take(200).collect());
                    }
                    texts.push("content", text);
                }
                "assistant/message" => {
                    if let Some(message) = data.get("message") {
                        let (text, reasoning) =
                            block_text(message.get("content").unwrap_or(&Value::Null));
                        if !text.trim().is_empty() {
                            texts.push("content", text);
                        }
                        if !reasoning.trim().is_empty() {
                            texts.push("reasoning", reasoning);
                        }
                        if let Some(source) = message.get("source") {
                            push_model_context(
                                &mut models,
                                clean_str(source.get("provider")),
                                clean_str(source.get("model")),
                                None,
                            );
                        }
                    }
                    if let Some(usage) = data.get("usage") {
                        let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                        let input = get("inputTokens");
                        let output = get("outputTokens");
                        let reasoning = get("reasoningTokens");
                        let cache = get("cacheReadTokens") + get("cacheWriteTokens");
                        tokens.input_tokens += input;
                        tokens.output_tokens += output + reasoning;
                        tokens.cache_tokens += cache;
                        tokens.total_tokens += input + output + reasoning + cache;
                    }
                }
                "tool/call" => {
                    let name = clean_str(data.get("name")).unwrap_or_else(|| "tool".to_string());
                    let args = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                    texts.push("tool", format!("{} {}", name, args));
                }
                "tool/result" => {
                    let output = data.get("message").map(tool_content).unwrap_or_default();
                    texts.push("tool", output);
                }
                _ => {}
            }
        }
        (title, first_prompt, texts.into_texts(), models, tokens)
    }

    fn event_batches(events: &[Value], min_level: &str) -> Vec<(usize, Vec<SessionRecord>)> {
        let mut batches = Vec::new();
        let mut emitted_calls: HashSet<String> = HashSet::new();
        for (index, event) in events.iter().enumerate() {
            let mut records = Vec::new();
            Self::build_event(event, min_level, &mut emitted_calls, &mut records);
            if !records.is_empty() {
                for (offset, record) in records.iter_mut().enumerate() {
                    record.byte_offset = index as u64 * 1000 + offset as u64;
                }
                batches.push((index, records));
            }
        }
        batches
    }

    fn build_event(
        event: &Value,
        min_level: &str,
        emitted_calls: &mut HashSet<String>,
        out: &mut Vec<SessionRecord>,
    ) {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        let data = event.get("data").unwrap_or(&Value::Null);
        let timestamp = event.get("time").and_then(Value::as_i64).map(fmt_ms_full);
        let group_id = || {
            let turn = data.get("turn").and_then(Value::as_u64);
            let step = data.get("step").and_then(Value::as_u64);
            match (turn, step) {
                (Some(turn), Some(step)) => Some(format!("{turn}:{step}")),
                _ => None,
            }
        };
        let mut emit = |record: SessionRecord| {
            if keep(&record.level, min_level) {
                out.push(record);
            }
        };

        match event_type {
            "user/message" => {
                let (text, _) = block_text(data.get("content").unwrap_or(&Value::Null));
                if text.trim().is_empty() {
                    return;
                }
                let human = data
                    .pointer("/source/kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "user");
                let mut record = rec(
                    if human { "user" } else { "meta" },
                    text,
                    "content",
                    &timestamp,
                );
                record.group_id = group_id();
                emit(record);
            }
            "assistant/message" => {
                let Some(message) = data.get("message") else {
                    return;
                };
                let (text, reasoning) = block_text(message.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    let mut record = rec("assistant", text, "content", &timestamp);
                    record.group_id = group_id();
                    emit(record);
                }
                if !reasoning.trim().is_empty() {
                    let mut record = rec("thinking", reasoning, "content", &timestamp);
                    record.group_id = group_id();
                    emit(record);
                }
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool-call") {
                            continue;
                        }
                        let call_id = clean_str(block.get("id")).unwrap_or_default();
                        if call_id.is_empty() || !emitted_calls.insert(call_id.clone()) {
                            continue;
                        }
                        let name =
                            clean_str(block.get("name")).unwrap_or_else(|| "tool".to_string());
                        let raw = block.get("arguments").and_then(Value::as_str).unwrap_or("");
                        let mut record =
                            tool_record(&name, &call_id, arguments_value(raw), &timestamp);
                        record.group_id = group_id();
                        emit(record);
                    }
                }
            }
            "tool/call" => {
                let call_id = clean_str(data.get("callId"));
                let name = clean_str(data.get("name")).unwrap_or_else(|| "tool".to_string());
                if let Some(call_id) = call_id {
                    if !emitted_calls.insert(call_id.clone()) {
                        return;
                    }
                    let raw = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                    let mut record = tool_record(&name, &call_id, arguments_value(raw), &timestamp);
                    record.group_id = group_id();
                    emit(record);
                }
            }
            "tool/result" => {
                let Some(message) = data.get("message") else {
                    return;
                };
                let call_id = clean_str(message.get("toolCallId"))
                    .or_else(|| clean_str(data.pointer("/error/callId")))
                    .unwrap_or_default();
                let output = tool_content(message);
                let mut record = rec("tool_result", output, "tool", &timestamp);
                record.tool_use_id = (!call_id.is_empty()).then_some(call_id);
                record.group_id = group_id();
                if let Some(meta) = data.get("meta").filter(|value| value.is_object()) {
                    record.result_meta = Some(meta.clone());
                }
                if message.get("isError").and_then(Value::as_bool) == Some(true)
                    && record.result_meta.is_none()
                {
                    record.result_meta = Some(json!({ "terminal": { "exit_code": 1 } }));
                }
                emit(record);
            }
            "todo/write" => {
                let mut record = rec("assistant", String::new(), "tool", &timestamp);
                record.tool_name = Some("TodoWrite".to_string());
                record.tool_input = Some(json!({ "todos": data.get("todos") }));
                record.tool_use_id = Some(format!(
                    "todo-{}",
                    event.get("seq").and_then(Value::as_i64).unwrap_or(0)
                ));
                emit(record);
            }
            "compaction/summary" => {
                let summary = clean_str(data.get("summary")).unwrap_or_default();
                let content = if summary.is_empty() {
                    "🗜 上下文已压缩".to_string()
                } else {
                    format!("🗜 上下文已压缩\n\n{summary}")
                };
                emit(rec("meta", content, "content", &timestamp));
            }
            "turn/end" => {
                let reason = data.pointer("/reason/kind").and_then(Value::as_str);
                if matches!(
                    reason,
                    Some("error") | Some("aborted") | Some("interrupted")
                ) {
                    let content = data
                        .pointer("/reason/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or(reason.unwrap_or("turn ended"));
                    emit(rec("meta", content.to_string(), "debug", &timestamp));
                }
            }
            _ => {}
        }
    }

    fn record_count(session: &DshSessionMeta) -> u32 {
        Self::load_events(&session.file)
            .map(|events| events.len() as u32)
            .unwrap_or(0)
    }

    fn child_descriptor(
        session: &DshSessionMeta,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let Ok(events) = Self::load_events(&session.file) else {
            return (None, None, None);
        };
        for event in events {
            if event.get("type").and_then(Value::as_str) != Some("subagent/descriptor") {
                continue;
            }
            let data = event.get("data").unwrap_or(&Value::Null);
            return (
                clean_str(data.get("label")),
                clean_str(data.get("provider")),
                clean_str(data.get("mode")),
            );
        }
        (None, None, None)
    }

    /// Map a child id to the parent tool call that spawned it. DSH records the parent
    /// `tool/call` name `subagent` (with `description`) and a `subagent/catalog` entry carrying
    /// the child id + label; the shared label is the stable link.
    fn subagent_links(
        parent_id: &str,
        descendants: &[DshSessionMeta],
        all: &[DshSessionMeta],
    ) -> HashMap<String, (String, String)> {
        let mut labels: HashMap<String, String> = HashMap::new();
        for child in descendants {
            let (label, provider, mode) = Self::child_descriptor(child);
            if let Some(label) = label {
                labels.insert(
                    label,
                    provider.or(mode).unwrap_or_else(|| "subagent".to_string()),
                );
            }
        }
        let mut calls: HashMap<String, String> = HashMap::new();
        let mut scan = |session_id: &str| {
            if let Some(meta) = all.iter().find(|session| session.id == session_id) {
                if let Ok(events) = Self::load_events(&meta.file) {
                    for event in events {
                        if event.get("type").and_then(Value::as_str) != Some("tool/call") {
                            continue;
                        }
                        let data = event.get("data").unwrap_or(&Value::Null);
                        if data.get("name").and_then(Value::as_str) != Some("subagent") {
                            continue;
                        }
                        let Some(call_id) = clean_str(data.get("callId")) else {
                            continue;
                        };
                        let raw = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                        if let Some(description) = serde_json::from_str::<Value>(raw)
                            .ok()
                            .and_then(|value| clean_str(value.get("description")))
                        {
                            calls.insert(description, call_id);
                        }
                    }
                }
            }
        };
        scan(parent_id);
        for child in descendants {
            if let Some(parent) = child.parent_session.as_deref() {
                if parent != parent_id {
                    scan(parent);
                }
            }
        }

        let mut links = HashMap::new();
        for child in descendants {
            let (label, provider, mode) = Self::child_descriptor(child);
            let agent_type = provider
                .or(mode.clone())
                .or_else(|| child.agent_preset.clone())
                .unwrap_or_else(|| "subagent".to_string());
            if let Some(call_id) = label.as_ref().and_then(|label| calls.get(label)).cloned() {
                links.insert(child.id.clone(), (call_id, agent_type));
            } else if let Some(label) = label {
                // Keep the label visible even when no spawning call survived in the log.
                links.insert(child.id.clone(), (String::new(), agent_type));
                labels.insert(child.id.clone(), label);
            }
        }
        links
    }
}

fn keep(level: &str, min_level: &str) -> bool {
    match min_level {
        "content" => level == "content",
        "tool" => level != "debug",
        _ => true,
    }
}

fn rec(record_type: &str, content: String, level: &str, ts: &Option<String>) -> SessionRecord {
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

fn tool_record(name: &str, call_id: &str, input: Value, ts: &Option<String>) -> SessionRecord {
    let display = if name == "subagent" { "Agent" } else { name };
    let level = if name == "question" {
        "content"
    } else {
        "tool"
    };
    let mut record = rec("assistant", String::new(), level, ts);
    record.tool_name = Some(display.to_string());
    record.tool_use_id = Some(call_id.to_string());
    record.tool_input = Some(input);
    record
}

impl AgentProvider for DshProvider {
    fn id(&self) -> &'static str {
        "dsh"
    }

    fn display_name(&self) -> &'static str {
        "DeepSeek Harness"
    }

    fn available(&self) -> bool {
        self.dsh_home.exists() || self.archive_root.exists()
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.sessions_root.clone(), self.archive_root.clone()]
    }

    fn capabilities(&self) -> Capabilities {
        let has_sessions = self.sessions_root.exists() || self.archive_root.exists();
        Capabilities {
            sessions_read: has_sessions,
            sessions_search: has_sessions,
            sessions_resume: self.sessions_root.exists(),
            sessions_subagents: has_sessions,
            rules_read: false,
            rules_write: false,
            memory_read: false,
            memory_write: false,
            instructions_read: true,
            instructions_write: true,
            archive_read: true,
            archive_write: true,
            config_format: "json",
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

    fn list_sessions(&self, project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
        let sessions = self.scan_all();
        let mut out: Vec<SessionSummary> = sessions
            .iter()
            .filter(|session| !session.is_subagent)
            .filter(|session| project.is_none_or(|project| session.project == project))
            .map(|session| Self::summary_json(session, &sessions))
            .collect();
        out.sort_by(|a, b| {
            b.timestamp
                .as_deref()
                .unwrap_or("")
                .cmp(a.timestamp.as_deref().unwrap_or(""))
        });
        Ok(out)
    }

    fn index_documents(&self) -> IndexBatch {
        let sessions = self.scan_all();
        let docs: Vec<IndexDoc> = sessions
            .par_iter()
            .filter(|session| !session.is_subagent)
            .map(|session| self.index_for(session, &sessions))
            .collect();
        IndexBatch { docs, failed: 0 }
    }

    fn index_manifest(&self) -> Vec<IndexManifestEntry> {
        self.scan_all()
            .into_iter()
            .filter(|session| !session.is_subagent)
            .map(|session| IndexManifestEntry {
                key: session.file.to_string_lossy().to_string(),
                version: format!("{}:{}", session.size, session.updated_ms),
            })
            .collect()
    }

    fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
        let sessions = self.scan_all();
        let docs: Vec<IndexDoc> = sessions
            .par_iter()
            .filter(|session| !session.is_subagent)
            .filter(|session| only.contains(&session.file.to_string_lossy().to_string()))
            .map(|session| self.index_for(session, &sessions))
            .collect();
        IndexBatch { docs, failed: 0 }
    }

    fn global_instruction_candidates(&self) -> Vec<InstructionCandidate> {
        vec![
            InstructionCandidate {
                title: "全局 DSH AGENTS.md".to_string(),
                scope: "global",
                kind: "instructions",
                path: self.dsh_home.join("AGENTS.md"),
                editable: true,
                include_missing: true,
                exists: None,
                size_bytes: None,
                description: "DSH 用户级 AGENTS.md 指令文件。".to_string(),
            },
            InstructionCandidate {
                title: "共享 agents AGENTS.md".to_string(),
                scope: "global",
                kind: "instructions",
                path: self.agents_home.join("AGENTS.md"),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "DSH 共享 agents 根目录的 AGENTS.md。".to_string(),
            },
        ]
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
                description: "DSH 项目指令文件。".to_string(),
            },
            InstructionCandidate {
                title: "项目 CLAUDE.md".to_string(),
                scope: "project",
                kind: "instructions",
                path: project_path.join("CLAUDE.md"),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "DSH 兼容的 CLAUDE.md 项目指令文件。".to_string(),
            },
        ]
    }

    fn instruction_project_roots(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for session in self.scan_all() {
            if session.project_path.is_empty() || !seen.insert(session.project_path.clone()) {
                continue;
            }
            roots.push(PathBuf::from(session.project_path));
        }
        roots
    }

    fn resume_command(&self, session_id: &str, extra_args: &[String]) -> Option<String> {
        // The TUI is the only DSH frontend with a per-session CLI flag; web/desktop installs
        // override this through the generic per-agent custom resume template in settings.
        Some(
            format!(
                "dsh tui --resume {} {}",
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
        let session = self.session_by_id(session_id, archive)?;
        let events = Self::load_events(&session.file).ok()?;
        for event in events {
            if event.get("type").and_then(Value::as_str) != Some("user/message") {
                continue;
            }
            let data = event.get("data").unwrap_or(&Value::Null);
            if data.pointer("/source/kind").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let (text, _) = block_text(data.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                return Some(text.chars().take(200).collect());
            }
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
        let session = self
            .session_by_id(session_id, archive)
            .ok_or_else(|| AppError::NotFound(format!("DSH session not found: {session_id}")))?;
        let events = Self::load_events(&session.file)?;
        let total = events.len();
        let start = (byte_offset as usize).min(total);
        if limit == 0 {
            return Ok(PaginatedRecords {
                records: Vec::new(),
                start_byte_offset: start as u64,
                next_byte_offset: start as u64,
                has_earlier: start > 0,
                has_more: start < total,
            });
        }
        let batches = Self::event_batches(&events[start..], min_level);
        let mut records = Vec::new();
        let mut cursor = start;
        for (index, batch) in batches {
            cursor = start + index + 1;
            records.extend(batch);
            if records.len() >= limit as usize {
                break;
            }
        }
        if records.len() < limit as usize {
            cursor = total;
        }
        Ok(PaginatedRecords {
            records,
            start_byte_offset: start as u64,
            next_byte_offset: cursor as u64,
            has_earlier: start > 0,
            has_more: cursor < total,
        })
    }

    fn session_tail(
        &self,
        project: &str,
        session_id: &str,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let session = self
            .session_by_id(session_id, archive)
            .ok_or_else(|| AppError::NotFound(format!("DSH session not found: {session_id}")))?;
        let total = Self::load_events(&session.file)?.len() as u64;
        self.session_before(project, session_id, total, limit, min_level, archive)
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
        let session = self
            .session_by_id(session_id, archive)
            .ok_or_else(|| AppError::NotFound(format!("DSH session not found: {session_id}")))?;
        let events = Self::load_events(&session.file)?;
        let total = events.len();
        let boundary = (before_offset as usize).min(total);
        if limit == 0 || boundary == 0 {
            return Ok(PaginatedRecords {
                records: Vec::new(),
                start_byte_offset: boundary as u64,
                next_byte_offset: boundary as u64,
                has_earlier: false,
                has_more: boundary < total,
            });
        }
        let target = limit as usize;
        let mut window = ((limit as usize).saturating_mul(3)).max(64).min(boundary);
        loop {
            let start = boundary.saturating_sub(window);
            let batches = Self::event_batches(&events[start..boundary], min_level);
            let mut selected_start = batches.len();
            let mut count = 0;
            while selected_start > 0 && count < target {
                selected_start -= 1;
                count += batches[selected_start].1.len();
            }
            let at_start = start == 0;
            if at_start || (count >= target && selected_start > 0) {
                let has_earlier = selected_start > 0;
                let start_byte_offset = batches
                    .get(selected_start)
                    .map(|(index, _)| (start + index) as u64)
                    .unwrap_or(0);
                let records = batches
                    .into_iter()
                    .skip(selected_start)
                    .flat_map(|(_, records)| records)
                    .collect();
                return Ok(PaginatedRecords {
                    records,
                    start_byte_offset,
                    next_byte_offset: boundary as u64,
                    has_earlier,
                    has_more: boundary < total,
                });
            }
            window = window.saturating_mul(2).min(boundary);
        }
    }

    fn list_subagents(
        &self,
        _project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SubagentInfo>, AppError> {
        let all = self.scan_all();
        let mut descendants = Vec::new();
        let mut queue = VecDeque::from([session_id.to_string()]);
        let mut seen = HashSet::from([session_id.to_string()]);
        while let Some(parent) = queue.pop_front() {
            for session in &all {
                if session.parent_session.as_deref() != Some(parent.as_str())
                    || !seen.insert(session.id.clone())
                {
                    continue;
                }
                if session.archive_name.as_deref() == archive.filter(|name| !name.is_empty()) {
                    descendants.push(session.clone());
                    queue.push_back(session.id.clone());
                }
            }
        }
        if descendants.is_empty() {
            return Ok(Vec::new());
        }
        let links = Self::subagent_links(session_id, &descendants, &all);
        let mut out = Vec::new();
        for child in descendants {
            let (label, _, _) = Self::child_descriptor(&child);
            let (tool_use_id, agent_type) = links
                .get(&child.id)
                .cloned()
                .unwrap_or_else(|| (String::new(), "subagent".to_string()));
            out.push(SubagentInfo {
                agent_id: child.id.clone(),
                agent_type,
                description: label
                    .or(child.agent_preset.clone())
                    .unwrap_or_else(|| child.id.clone()),
                tool_use_id,
                record_count: Self::record_count(&child),
                parent_agent_id: child
                    .parent_session
                    .clone()
                    .filter(|parent| parent != session_id),
            });
        }
        Ok(out)
    }

    fn subagent_detail(
        &self,
        project: &str,
        _session_id: &str,
        agent_id: &str,
        byte_offset: u64,
        limit: u32,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        self.session_detail(project, agent_id, byte_offset, limit, "tool", archive)
    }

    fn search_in_session(
        &self,
        _project: &str,
        session_id: &str,
        query: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let session = self
            .session_by_id(session_id, archive)
            .ok_or_else(|| AppError::NotFound(format!("DSH session not found: {session_id}")))?;
        let events = Self::load_events(&session.file)?;
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
            let data = event.get("data").unwrap_or(&Value::Null);
            let mut candidates: Vec<(&str, String)> = Vec::new();
            match event_type {
                "user/message" => {
                    let (text, _) = block_text(data.get("content").unwrap_or(&Value::Null));
                    candidates.push(("message", text));
                }
                "assistant/message" => {
                    if let Some(message) = data.get("message") {
                        let (text, reasoning) =
                            block_text(message.get("content").unwrap_or(&Value::Null));
                        if !text.is_empty() {
                            candidates.push(("message", text));
                        }
                        if !reasoning.is_empty() {
                            candidates.push(("thinking", reasoning));
                        }
                    }
                }
                "tool/call" => {
                    let name = data.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                    candidates.push(("tool_result", format!("{} {}", name, args)));
                }
                "tool/result" => {
                    let output = data.get("message").map(tool_content).unwrap_or_default();
                    candidates.push(("tool_result", output));
                }
                _ => {}
            }
            for (record_type, text) in candidates {
                if text.is_empty() || !text.to_lowercase().contains(&needle) {
                    continue;
                }
                let timestamp = event.get("time").and_then(Value::as_i64).map(fmt_ms_full);
                hits.push(SessionSearchHit {
                    byte_offset: index as u64,
                    snippet: crate::services::search::extract_snippet(&text, &needle),
                    record_type: record_type.to_string(),
                    timestamp,
                });
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, PathBuf) {
        let id = DIR_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("code-dejavu-dsh-{}-{}", std::process::id(), id));
        let project = root.join("sessions").join("--C--Codes-demo--");
        let session = project.join("session-1");
        fs::create_dir_all(&session).expect("session dir");
        let header = json!({
            "type": "session", "version": 4, "id": "dsh-session-1",
            "createdAt": 1_700_000_000_000_i64, "cwd": "C:\\Codes\\demo",
            "isSeeded": false, "delegationDepth": 0
        });
        let events = [
            json!({"type":"turn/start","data":{"turn":1},"seq":0,"time":1_700_000_000_100_i64}),
            json!({"type":"user/message","data":{"content":[{"type":"text","text":"Hello DSH"}],"source":{"kind":"user"},"role":"user","id":"u1"},"seq":1,"time":1_700_000_000_200_i64,"surfaceOp":"append"}),
            json!({"type":"assistant/message","data":{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"text","text":"Working on it"},{"type":"tool-call","id":"call-1","name":"bash","arguments":"{\"command\":\"ls\"}"}],"source":{"kind":"model","provider":"deepseek","model":"deepseek-v4"},"id":"a1"},"usage":{"inputTokens":10,"outputTokens":5}},"seq":2,"time":1_700_000_000_300_i64,"surfaceOp":"append"}),
            json!({"type":"tool/call","data":{"turn":1,"step":1,"callId":"call-1","name":"bash","arguments":"{\"command\":\"ls\"}"},"seq":3,"time":1_700_000_000_400_i64}),
            json!({"type":"tool/result","data":{"turn":1,"step":1,"message":{"role":"tool","toolCallId":"call-1","content":[{"type":"text","text":"file.txt"}],"source":{"kind":"tool","callId":"call-1"},"id":"t1"},"meta":{"terminal":{"exit_code":0}}},"seq":4,"time":1_700_000_000_500_i64,"surfaceOp":"append"}),
            json!({"type":"session/title","data":{"title":"FSH demo"},"seq":5,"time":1_700_000_000_600_i64}),
        ];
        let raw = std::iter::once(header.to_string())
            .chain(events.into_iter().map(|event| event.to_string()))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(session.join("session.v4.jsonl"), raw).expect("raw session");

        // A second session stored as a single Zstandard frame, plus a child subagent.
        let session2 = project.join("session-2");
        fs::create_dir_all(&session2).expect("session2");
        let header2 = json!({
            "type": "session", "version": 4, "id": "dsh-session-2",
            "createdAt": 1_700_000_001_000_i64, "cwd": "C:\\Codes\\demo",
            "isSeeded": false, "delegationDepth": 0
        });
        let child_header = json!({
            "type": "session", "version": 4, "id": "dsh-child-1",
            "createdAt": 1_700_000_002_000_i64, "cwd": "C:\\Codes\\demo",
            "parentSession": "dsh-session-2", "origin": "subagent",
            "isSeeded": false, "delegationDepth": 1
        });
        let header_frame = header2.to_string() + "\n";
        let event_frame = [
            json!({"type":"tool/call","data":{"turn":1,"step":1,"callId":"delegate-1","name":"subagent","arguments":"{\"description\":\"child task\"}"},"seq":0,"time":1_700_000_001_100_i64}).to_string(),
            json!({"type":"subagent/catalog","data":{"childId":"dsh-child-1","label":"child task"},"seq":1,"time":1_700_000_001_200_i64}).to_string(),
            json!({"type":"assistant/message","data":{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"text","text":"Delegated"}],"source":{"kind":"model","provider":"deepseek","model":"deepseek-v4"}}},"seq":2,"time":1_700_000_001_300_i64}).to_string(),
        ]
        .join("\n")
            + "\n";
        // DSH writes one checksummed frame for the header and one per append batch. Concatenated
        // frames must decode as one logical stream.
        let compress = |text: &str| {
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).expect("encoder");
            encoder.write_all(text.as_bytes()).expect("write");
            encoder.finish().expect("finish")
        };
        let mut file_bytes = compress(&header_frame);
        file_bytes.extend(compress(&event_frame));
        fs::write(session2.join("session.v4.jsonl.zstd"), file_bytes).expect("zstd session");

        let child = project.join("dsh-child-1");
        fs::create_dir_all(&child).expect("child dir");
        let child_log = [
            child_header.to_string(),
            json!({"type":"subagent/descriptor","data":{"label":"child task","provider":"spawn","mode":"continuable"},"seq":0,"time":1_700_000_002_100_i64}).to_string(),
            json!({"type":"assistant/message","data":{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"text","text":"child answer"}],"source":{"kind":"model","provider":"deepseek","model":"deepseek-v4"}}},"seq":1,"time":1_700_000_002_200_i64}).to_string(),
        ]
        .join("\n")
            + "\n";
        fs::write(child.join("session.v4.jsonl"), child_log).expect("child log");
        (root, project.join("session-1").join("session.v4.jsonl"))
    }

    #[test]
    fn reads_raw_and_zstd_sessions_and_pages_events() {
        let (root, _) = fixture();
        let provider = DshProvider::from_home(Host::Native, root.clone(), root.join("agents"));
        let sessions = provider.list_sessions(None).expect("sessions");
        assert_eq!(sessions.len(), 2);
        let session = sessions
            .iter()
            .find(|session| session.session_id == "dsh-session-1")
            .expect("session 1");
        assert_eq!(session.subagent_count, 0);

        let detail = provider
            .session_detail("", "dsh-session-1", 0, 100, "tool", None)
            .expect("detail");
        assert!(detail
            .records
            .iter()
            .any(|r| r.content_preview == "Hello DSH"));
        assert!(detail
            .records
            .iter()
            .any(|r| r.tool_name.as_deref() == Some("bash")));
        assert!(detail
            .records
            .iter()
            .any(|r| r.content_preview == "file.txt"));

        let tail = provider
            .session_tail("", "dsh-session-1", 2, "content", None)
            .expect("tail");
        assert!(tail
            .records
            .iter()
            .any(|r| r.content_preview == "Working on it"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_zstd_session_and_subagent_linkage() {
        let (root, _) = fixture();
        let provider = DshProvider::from_home(Host::Native, root.clone(), root.join("agents"));
        let sessions = provider.list_sessions(None).expect("sessions");
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "dsh-session-2"));
        let subagents = provider
            .list_subagents("", "dsh-session-2", None)
            .expect("subagents");
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].agent_id, "dsh-child-1");
        assert_eq!(subagents[0].tool_use_id, "delegate-1");
        assert_eq!(subagents[0].description, "child task");
        let detail = provider
            .subagent_detail("", "dsh-session-2", "dsh-child-1", 0, 100, None)
            .expect("child detail");
        assert!(detail
            .records
            .iter()
            .any(|r| r.content_preview == "child answer"));
        let _ = fs::remove_dir_all(root);
    }
}
