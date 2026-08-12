//! Pi coding-agent provider for the append-only session trees stored under `~/.pi/agent`.

use super::{
    metadata_pool, quote_command_arg, AgentProvider, Capabilities, FastIndexTextCollector,
    IndexBatch, IndexDoc, IndexManifestEntry, LineParser, TokenUsage,
};
use crate::error::AppError;
use crate::hosts::Host;
use crate::models::profile::ProfileArchive;
use crate::models::session::{
    push_model_context, PaginatedRecords, SessionModelInfo, SessionRecord, SessionSearchHit,
    SessionSummary,
};
use crate::paths::app_data_dir;
use crate::services::jsonl;
use crate::services::profile_archiver::{self, SnapshotItem, SnapshotSpec};
use chrono::{DateTime, Local};
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

fn home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn fmt_ts(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.replace('T', " ").chars().take(19).collect())
}

fn message_ts(entry: &Value) -> Option<String> {
    message_activity_millis(entry)
        .and_then(DateTime::from_timestamp_millis)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
}

fn message_activity_millis(entry: &Value) -> Option<i64> {
    entry
        .pointer("/message/timestamp")
        .and_then(Value::as_i64)
        .or_else(|| {
            entry
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
                .map(|timestamp| timestamp.timestamp_millis())
        })
}

fn file_version(metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("{}:{}", metadata.len(), modified)
}

fn clean_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block.get("text").and_then(Value::as_str).map(String::from),
                Some("image") => Some(
                    block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(|mime| format!("[image: {mime}]"))
                        .unwrap_or_else(|| "[image]".to_string()),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn usage_from(value: Option<&Value>) -> TokenUsage {
    let Some(value) = value else {
        return TokenUsage::default();
    };
    let get = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let input = get("input");
    let output = get("output");
    let cache = get("cacheRead").saturating_add(get("cacheWrite"));
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_tokens: cache,
        total_tokens: value
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| input.saturating_add(output).saturating_add(cache)),
    }
}

fn add_usage(total: &mut TokenUsage, usage: TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_tokens = total.cache_tokens.saturating_add(usage.cache_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
}

fn dominated(level: &str, min_level: &str) -> bool {
    match min_level {
        "content" => level != "content",
        "tool" => level == "debug",
        _ => false,
    }
}

fn record(
    record_type: &str,
    content_preview: String,
    timestamp: Option<String>,
    level: &str,
) -> SessionRecord {
    SessionRecord {
        record_type: record_type.to_string(),
        content_preview,
        timestamp,
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

struct PiParser;

impl PiParser {
    fn timestamp(value: &Value) -> Option<String> {
        message_ts(value)
    }

    fn message_records(value: &Value, min_level: &str) -> Vec<SessionRecord> {
        let Some(message) = value.get("message") else {
            return Vec::new();
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let timestamp = Self::timestamp(value);
        match role {
            "user" => {
                if dominated("content", min_level) {
                    Vec::new()
                } else {
                    vec![record(
                        "user",
                        content_text(message.get("content")),
                        timestamp,
                        "content",
                    )]
                }
            }
            "assistant" => {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    return Vec::new();
                };
                let mut records = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" if !dominated("content", min_level) => records.push(record(
                            "assistant",
                            block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            timestamp.clone(),
                            "content",
                        )),
                        "thinking" if !dominated("content", min_level) => records.push(record(
                            "thinking",
                            block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            timestamp.clone(),
                            "content",
                        )),
                        "toolCall" if !dominated("tool", min_level) => {
                            let mut tool =
                                record("assistant", String::new(), timestamp.clone(), "tool");
                            tool.tool_name = clean_string(block.get("name"));
                            tool.tool_use_id = clean_string(block.get("id"));
                            tool.tool_input = block.get("arguments").cloned();
                            records.push(tool);
                        }
                        _ => {}
                    }
                }
                records
            }
            "toolResult" if !dominated("tool", min_level) => {
                let mut result = record(
                    "tool_result",
                    content_text(message.get("content")),
                    timestamp,
                    "tool",
                );
                result.tool_name = clean_string(message.get("toolName"));
                result.tool_use_id = clean_string(message.get("toolCallId"));
                result.result_meta = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .map(|is_error| json!({ "is_error": is_error }));
                vec![result]
            }
            "bashExecution" if !dominated("tool", min_level) => {
                let entry_id = value.get("id").and_then(Value::as_str).unwrap_or("bash");
                let tool_id = format!("pi-bash-{entry_id}");
                let mut call = record("assistant", String::new(), timestamp.clone(), "tool");
                call.tool_name = Some("bash".to_string());
                call.tool_use_id = Some(tool_id.clone());
                call.tool_input = Some(json!({
                    "command": message.get("command").and_then(Value::as_str).unwrap_or("")
                }));

                let mut result = record(
                    "tool_result",
                    message
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    timestamp,
                    "tool",
                );
                result.tool_name = Some("bash".to_string());
                result.tool_use_id = Some(tool_id);
                result.result_meta = Some(json!({
                    "terminal": {
                        "exit_code": message.get("exitCode").cloned().unwrap_or(Value::Null),
                        "cancelled": message.get("cancelled").and_then(Value::as_bool).unwrap_or(false),
                        "truncated": message.get("truncated").and_then(Value::as_bool).unwrap_or(false)
                    }
                }));
                vec![call, result]
            }
            "custom" if message.get("display").and_then(Value::as_bool) != Some(false) => {
                if dominated("content", min_level) {
                    Vec::new()
                } else {
                    vec![record(
                        "meta",
                        content_text(message.get("content")),
                        timestamp,
                        "content",
                    )]
                }
            }
            "branchSummary" | "compactionSummary" if !dominated("content", min_level) => {
                vec![record(
                    "meta",
                    message
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    timestamp,
                    "content",
                )]
            }
            _ if min_level == "debug" => vec![record(
                "meta",
                serde_json::to_string(message).unwrap_or_default(),
                timestamp,
                "debug",
            )],
            _ => Vec::new(),
        }
    }
}

impl LineParser for PiParser {
    fn reset(&mut self) {}

    fn push(&mut self, value: &Value, min_level: &str) -> Vec<SessionRecord> {
        let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if entry_type == "message" {
            return Self::message_records(value, min_level);
        }
        if entry_type == "custom_message" {
            if value.get("display").and_then(Value::as_bool) == Some(false)
                || dominated("content", min_level)
            {
                return Vec::new();
            }
            return vec![record(
                "meta",
                content_text(value.get("content")),
                Self::timestamp(value),
                "content",
            )];
        }
        if matches!(entry_type, "compaction" | "branch_summary") {
            if dominated("content", min_level) {
                return Vec::new();
            }
            return vec![record(
                "meta",
                value
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Self::timestamp(value),
                "content",
            )];
        }
        if dominated("debug", min_level) {
            return Vec::new();
        }
        let preview = match entry_type {
            "session" => format!(
                "Pi session {} · {}",
                value.get("id").and_then(Value::as_str).unwrap_or(""),
                value.get("cwd").and_then(Value::as_str).unwrap_or("")
            ),
            "model_change" => format!(
                "Model: {}/{}",
                value.get("provider").and_then(Value::as_str).unwrap_or(""),
                value.get("modelId").and_then(Value::as_str).unwrap_or("")
            ),
            "thinking_level_change" => format!(
                "Thinking level: {}",
                value
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            "session_info" => format!(
                "Session name: {}",
                value.get("name").and_then(Value::as_str).unwrap_or("")
            ),
            "label" => format!(
                "Label: {}",
                value.get("label").and_then(Value::as_str).unwrap_or("")
            ),
            "custom" => serde_json::to_string(value).unwrap_or_default(),
            _ => serde_json::to_string(value).unwrap_or_default(),
        };
        vec![record("meta", preview, Self::timestamp(value), "debug")]
    }

    fn flush(&mut self, _min_level: &str) -> Vec<SessionRecord> {
        Vec::new()
    }

    fn group_of(&self, value: &Value) -> Option<String> {
        clean_string(value.get("id"))
    }

    fn skippable(&self, _line: &str, _min_level: &str) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
struct PiSessionMeta {
    path: PathBuf,
    session_id: String,
    cwd: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    first_prompt: Option<String>,
    agent_title: Option<String>,
    model_contexts: Vec<SessionModelInfo>,
    tokens: TokenUsage,
    texts: Vec<super::IndexText>,
    file_size: u64,
    version: String,
}

pub struct PiProvider {
    agent_dir: PathBuf,
    archive_root: PathBuf,
    base_session_roots: Vec<PathBuf>,
    host: Host,
    home: PathBuf,
    discovered_session_roots: Mutex<HashSet<PathBuf>>,
}

impl Default for PiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PiProvider {
    pub fn new() -> Self {
        let home = home();
        let agent_dir = std::env::var("PI_CODING_AGENT_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| Self::configured_path(&value, &home, &Host::Native))
            .unwrap_or_else(|| home.join(".pi").join("agent"));
        let mut roots = vec![agent_dir.join("sessions")];
        if let Some(path) = std::env::var("PI_CODING_AGENT_SESSION_DIR")
            .ok()
            .and_then(|value| Self::configured_path(&value, &home, &Host::Native))
        {
            roots.push(path);
        }
        Self::from_parts(Host::Native, home, agent_dir, roots)
    }

    pub fn for_host(host: Host, home: &Path) -> Self {
        let agent_dir = home.join(".pi").join("agent");
        Self::from_parts(
            host,
            home.to_path_buf(),
            agent_dir.clone(),
            vec![agent_dir.join("sessions")],
        )
    }

    fn from_parts(host: Host, home: PathBuf, agent_dir: PathBuf, mut roots: Vec<PathBuf>) -> Self {
        if let Ok(settings) = fs::read_to_string(agent_dir.join("settings.json")) {
            if let Ok(value) = serde_json::from_str::<Value>(&settings) {
                if let Some(path) = value
                    .get("sessionDir")
                    .and_then(Value::as_str)
                    .and_then(|value| Self::configured_path(value, &home, &host))
                {
                    roots.push(path);
                }
            }
        }
        roots.sort();
        roots.dedup();
        let archive_root = match host.tag() {
            Some(_) => app_data_dir()
                .join("archives")
                .join("pi")
                .join(format!("wsl-{}", host.key())),
            None => app_data_dir().join("archives").join("pi"),
        };
        Self {
            agent_dir,
            archive_root,
            base_session_roots: roots,
            host,
            home,
            discovered_session_roots: Mutex::new(HashSet::new()),
        }
    }

    fn configured_path(value: &str, home: &Path, host: &Host) -> Option<PathBuf> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if value == "~" {
            return Some(home.to_path_buf());
        }
        if let Some(relative) = value
            .strip_prefix("~/")
            .or_else(|| value.strip_prefix("~\\"))
        {
            return Some(home.join(relative));
        }
        if matches!(host, Host::Wsl { .. }) && value.starts_with('/') {
            return Some(host.to_readable(value));
        }
        let path = PathBuf::from(value);
        Some(if path.is_absolute() {
            path
        } else {
            home.join(path)
        })
    }

    fn jsonl_files_in(root: &Path) -> Vec<PathBuf> {
        if !root.exists() {
            return Vec::new();
        }
        WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect()
    }

    fn snapshot_spec(&self) -> SnapshotSpec {
        SnapshotSpec {
            source: "pi",
            display_name: "PiAgent",
            archive_root: self.archive_root.clone(),
            items: vec![SnapshotItem {
                name: "pi",
                path: self.agent_dir.clone(),
                preserve: &["auth.json"],
            }],
            clear_current_on_create: true,
        }
    }

    fn archive_sessions_dir(&self, archive_name: &str) -> PathBuf {
        self.archive_root
            .join(archive_name)
            .join("pi")
            .join("sessions")
    }

    fn read_header(path: &Path) -> Option<(String, String, Option<String>)> {
        let file = File::open(path).ok()?;
        for line in BufReader::with_capacity(8 * 1024, file)
            .lines()
            .take(8)
            .map_while(Result::ok)
        {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("session") {
                return None;
            }
            return Some((
                clean_string(value.get("id"))?,
                value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                clean_string(value.get("timestamp")),
            ));
        }
        None
    }

    fn project_session_root(&self, cwd: &str) -> Option<PathBuf> {
        let project = self.host.to_readable(cwd);
        let settings = fs::read_to_string(project.join(".pi").join("settings.json")).ok()?;
        let value = serde_json::from_str::<Value>(&settings).ok()?;
        let session_dir = value.get("sessionDir").and_then(Value::as_str)?.trim();
        if session_dir.is_empty() {
            return None;
        }
        if session_dir == "~"
            || session_dir.starts_with("~/")
            || session_dir.starts_with("~\\")
            || Path::new(session_dir).is_absolute()
            || (matches!(self.host, Host::Wsl { .. }) && session_dir.starts_with('/'))
        {
            Self::configured_path(session_dir, &self.home, &self.host)
        } else {
            // Pi resolves project settings against the process cwd, not against the home dir.
            Some(project.join(session_dir))
        }
    }

    fn session_roots(&self) -> Vec<PathBuf> {
        let mut initial = self.base_session_roots.clone();
        if let Ok(discovered) = self.discovered_session_roots.lock() {
            initial.extend(discovered.iter().cloned());
        }
        let mut roots: VecDeque<PathBuf> = initial.into();
        let mut seen_roots = HashSet::new();
        let mut discovered = Vec::new();
        while let Some(root) = roots.pop_front() {
            let root_key = root.to_string_lossy().to_string();
            if !seen_roots.insert(root_key) {
                continue;
            }
            for path in Self::jsonl_files_in(&root) {
                if let Some((_, cwd, _)) = Self::read_header(&path) {
                    if let Some(project_root) = self.project_session_root(&cwd) {
                        roots.push_back(project_root);
                    }
                }
            }
            discovered.push(root);
        }
        if let Ok(mut cached) = self.discovered_session_roots.lock() {
            cached.extend(discovered.iter().cloned());
        }
        discovered
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut seen_files = HashSet::new();
        let mut files = Vec::new();
        for root in self.session_roots() {
            for path in Self::jsonl_files_in(&root) {
                let key = path.to_string_lossy().to_string();
                if !seen_files.insert(key) {
                    continue;
                }
                files.push(path);
            }
        }
        files
    }

    fn session_sources(&self) -> Vec<(PathBuf, Option<String>)> {
        let mut sources: Vec<_> = self
            .session_roots()
            .into_iter()
            .filter(|root| root.exists())
            .map(|root| (root, None))
            .collect();
        if let Ok(archives) = fs::read_dir(&self.archive_root) {
            for entry in archives.flatten() {
                if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }
                let archive_name = entry.file_name().to_string_lossy().to_string();
                let sessions_dir = entry.path().join("pi").join("sessions");
                if sessions_dir.exists() {
                    sources.push((sessions_dir, Some(archive_name)));
                }
            }
        }
        sources
    }

    fn all_session_files(&self) -> Vec<(PathBuf, Option<String>)> {
        let mut seen_files = HashSet::new();
        let mut files = Vec::new();
        for (root, archive_name) in self.session_sources() {
            for path in Self::jsonl_files_in(&root) {
                let key = path.to_string_lossy().to_string();
                if seen_files.insert(key) {
                    files.push((path, archive_name.clone()));
                }
            }
        }
        files
    }

    fn archive_name_for_path(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.archive_root)
            .ok()?
            .components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
    }

    fn scan_session(path: &Path) -> Option<PiSessionMeta> {
        let file = File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        let mut session_id = None;
        let mut cwd = String::new();
        let mut created_raw = None;
        let mut last_activity_millis = None;
        let mut first_prompt = None;
        let mut agent_title = None;
        let mut model_contexts = Vec::new();
        let mut tokens = TokenUsage::default();
        let mut texts = FastIndexTextCollector::default();
        let mut current_provider = None;
        let mut current_model = None;
        let mut thinking_level = None;

        for line in BufReader::with_capacity(128 * 1024, file)
            .lines()
            .map_while(Result::ok)
        {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            if session_id.is_none() {
                if entry_type != "session" {
                    return None;
                }
                session_id = clean_string(value.get("id"));
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                created_raw = clean_string(value.get("timestamp"));
                continue;
            }
            match entry_type {
                "session_info" => {
                    agent_title = clean_string(value.get("name"));
                }
                "thinking_level_change" => {
                    thinking_level = clean_string(value.get("thinkingLevel"));
                    if current_provider.is_some() || current_model.is_some() {
                        push_model_context(
                            &mut model_contexts,
                            current_provider.clone(),
                            current_model.clone(),
                            thinking_level.clone(),
                        );
                    }
                }
                "model_change" => {
                    current_provider = clean_string(value.get("provider"));
                    current_model = clean_string(value.get("modelId"));
                    push_model_context(
                        &mut model_contexts,
                        current_provider.clone(),
                        current_model.clone(),
                        thinking_level.clone(),
                    );
                }
                "compaction" | "branch_summary" => {
                    if let Some(summary) = clean_string(value.get("summary")) {
                        texts.push("content", summary);
                    }
                    add_usage(&mut tokens, usage_from(value.get("usage")));
                }
                "custom_message" => {
                    if value.get("display").and_then(Value::as_bool) != Some(false) {
                        texts.push("content", content_text(value.get("content")));
                    }
                }
                "message" => {
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    match message.get("role").and_then(Value::as_str).unwrap_or("") {
                        "user" => {
                            if let Some(timestamp) = message_activity_millis(&value) {
                                if last_activity_millis.is_none_or(|last| timestamp > last) {
                                    last_activity_millis = Some(timestamp);
                                }
                            }
                            let text = content_text(message.get("content"));
                            if first_prompt.is_none() && !text.trim().is_empty() {
                                first_prompt = Some(text.chars().take(200).collect());
                            }
                            texts.push("content", text);
                        }
                        "assistant" => {
                            if let Some(timestamp) = message_activity_millis(&value) {
                                if last_activity_millis.is_none_or(|last| timestamp > last) {
                                    last_activity_millis = Some(timestamp);
                                }
                            }
                            current_provider = clean_string(message.get("provider"));
                            current_model = clean_string(message.get("model"));
                            push_model_context(
                                &mut model_contexts,
                                current_provider.clone(),
                                current_model.clone(),
                                thinking_level.clone(),
                            );
                            add_usage(&mut tokens, usage_from(message.get("usage")));
                            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                                for block in blocks {
                                    match block.get("type").and_then(Value::as_str).unwrap_or("") {
                                        "text" => texts.push(
                                            "content",
                                            block
                                                .get("text")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string(),
                                        ),
                                        "thinking" => texts.push(
                                            "reasoning",
                                            block
                                                .get("thinking")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string(),
                                        ),
                                        "toolCall" => texts.push(
                                            "tool",
                                            format!(
                                                "{} {}",
                                                block
                                                    .get("name")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or(""),
                                                block
                                                    .get("arguments")
                                                    .map(Value::to_string)
                                                    .unwrap_or_default()
                                            ),
                                        ),
                                        _ => {}
                                    }
                                }
                            }
                        }
                        // Tool-result usage belongs to the tool implementation, not the model
                        // turn. Keep its content searchable but do not double-count model usage.
                        "toolResult" => {
                            texts.push(
                                "tool",
                                format!(
                                    "{} {}",
                                    message
                                        .get("toolName")
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                    content_text(message.get("content"))
                                ),
                            );
                        }
                        "bashExecution" => {
                            texts.push(
                                "tool",
                                message
                                    .get("command")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            );
                            texts.push(
                                "tool",
                                message
                                    .get("output")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            );
                        }
                        "custom"
                            if message.get("display").and_then(Value::as_bool) != Some(false) =>
                        {
                            texts.push("content", content_text(message.get("content")));
                        }
                        "branchSummary" | "compactionSummary" => {
                            if let Some(summary) = clean_string(message.get("summary")) {
                                texts.push("content", summary);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let session_id = session_id?;
        Some(PiSessionMeta {
            path: path.to_path_buf(),
            session_id,
            cwd,
            created_at: created_raw.as_deref().map(fmt_ts),
            updated_at: last_activity_millis
                .and_then(DateTime::from_timestamp_millis)
                .map(|timestamp| {
                    timestamp
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .or_else(|| created_raw.as_deref().map(fmt_ts)),
            first_prompt,
            agent_title,
            model_contexts,
            tokens,
            texts: texts.into_texts(),
            file_size: metadata.len(),
            version: file_version(&metadata),
        })
    }

    fn session_file(
        &self,
        project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Option<PathBuf> {
        let files = match archive.filter(|name| !name.trim().is_empty()) {
            Some(name) => Self::jsonl_files_in(&self.archive_sessions_dir(name)),
            None => self.session_files(),
        };
        files.into_iter().find(|path| {
            Self::read_header(path).is_some_and(|(id, cwd, _)| {
                id == session_id && (project.is_empty() || cwd == project)
            })
        })
    }

    fn to_summary(&self, meta: PiSessionMeta) -> SessionSummary {
        SessionSummary {
            source: "pi".to_string(),
            session_id: meta.session_id,
            project: meta.cwd.clone(),
            project_path: self
                .host
                .to_readable(&meta.cwd)
                .to_string_lossy()
                .to_string(),
            first_prompt: meta.first_prompt,
            agent_title: meta.agent_title,
            created_at: meta.created_at,
            updated_at: meta.updated_at.clone(),
            timestamp: meta.updated_at,
            file_size_bytes: meta.file_size,
            subagent_count: 0,
            archive_name: None,
            model_contexts: meta.model_contexts,
        }
    }

    fn to_doc(&self, meta: PiSessionMeta, archive_name: Option<String>) -> IndexDoc {
        IndexDoc {
            source: "pi".to_string(),
            session_id: meta.session_id,
            project: meta.cwd.clone(),
            project_path: self
                .host
                .to_readable(&meta.cwd)
                .to_string_lossy()
                .to_string(),
            created_at: meta.created_at,
            updated_at: meta.updated_at.clone(),
            agent_title: meta.agent_title,
            timestamp: meta.updated_at,
            file_size_bytes: meta.file_size,
            subagent_count: 0,
            archive_name,
            first_prompt: meta.first_prompt,
            model_contexts: meta.model_contexts,
            texts: meta.texts,
            tokens: meta.tokens,
            key: meta.path.to_string_lossy().to_string(),
            version: meta.version,
        }
    }
}

impl AgentProvider for PiProvider {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn display_name(&self) -> &'static str {
        "PiAgent"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions_read: true,
            sessions_search: true,
            sessions_resume: true,
            sessions_subagents: false,
            rules_read: false,
            rules_write: false,
            memory_read: false,
            memory_write: false,
            instructions_read: false,
            instructions_write: false,
            archive_read: true,
            archive_write: true,
            config_format: "json",
        }
    }

    fn available(&self) -> bool {
        self.agent_dir.exists()
            || self.archive_root.exists()
            || self.base_session_roots.iter().any(|root| root.exists())
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.base_session_roots.clone();
        if let Ok(discovered) = self.discovered_session_roots.lock() {
            roots.extend(discovered.iter().cloned());
        }
        roots.push(self.archive_root.clone());
        roots.sort();
        roots.dedup();
        roots
    }

    fn list_sessions(&self, project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
        let mut sessions: Vec<_> = metadata_pool().install(|| {
            self.session_files()
                .into_par_iter()
                .filter_map(|path| Self::scan_session(&path))
                .filter(|meta| project.is_none_or(|project| meta.cwd == project))
                .map(|meta| self.to_summary(meta))
                .collect()
        });
        sessions.sort_by(|left, right| {
            right
                .timestamp
                .as_deref()
                .unwrap_or("")
                .cmp(left.timestamp.as_deref().unwrap_or(""))
        });
        Ok(sessions)
    }

    fn index_documents(&self) -> IndexBatch {
        let results: Vec<_> = self
            .all_session_files()
            .into_par_iter()
            .map(|(path, archive_name)| (Self::scan_session(&path), archive_name))
            .collect();
        let failed = results
            .iter()
            .filter(|(result, _)| result.is_none())
            .count();
        let docs = results
            .into_iter()
            .filter_map(|(meta, archive_name)| meta.map(|meta| self.to_doc(meta, archive_name)))
            .collect();
        IndexBatch { docs, failed }
    }

    fn index_manifest(&self) -> Vec<IndexManifestEntry> {
        self.all_session_files()
            .into_iter()
            .filter_map(|(path, _)| {
                let metadata = fs::metadata(&path).ok()?;
                Some(IndexManifestEntry {
                    key: path.to_string_lossy().to_string(),
                    version: file_version(&metadata),
                })
            })
            .collect()
    }

    fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
        let results: Vec<_> = only
            .par_iter()
            .map(|path| {
                let path = Path::new(path);
                (Self::scan_session(path), self.archive_name_for_path(path))
            })
            .collect();
        let failed = results
            .iter()
            .filter(|(result, _)| result.is_none())
            .count();
        let docs = results
            .into_iter()
            .filter_map(|(meta, archive_name)| meta.map(|meta| self.to_doc(meta, archive_name)))
            .collect();
        IndexBatch { docs, failed }
    }

    fn resume_command(&self, session_id: &str, extra_args: &[String]) -> Option<String> {
        Some(
            format!(
                "pi --session {} {}",
                quote_command_arg(session_id),
                extra_args.join(" ")
            )
            .trim()
            .to_string(),
        )
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

    fn first_prompt(
        &self,
        project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Option<String> {
        let path = self.session_file(project, session_id, archive)?;
        Self::scan_session(&path)?.first_prompt
    }

    fn session_detail(
        &self,
        project: &str,
        session_id: &str,
        byte_offset: u64,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self
            .session_file(project, session_id, archive)
            .ok_or_else(|| {
                AppError::NotFound(format!("PiAgent session not found: {session_id}"))
            })?;
        jsonl::read_seekable_cached(&path, byte_offset, limit, min_level, &mut PiParser)
    }

    fn session_tail(
        &self,
        project: &str,
        session_id: &str,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self
            .session_file(project, session_id, archive)
            .ok_or_else(|| {
                AppError::NotFound(format!("PiAgent session not found: {session_id}"))
            })?;
        jsonl::read_tail_with(&path, limit, min_level, &mut PiParser)
    }

    fn session_before(
        &self,
        project: &str,
        session_id: &str,
        before_offset: u64,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self
            .session_file(project, session_id, archive)
            .ok_or_else(|| {
                AppError::NotFound(format!("PiAgent session not found: {session_id}"))
            })?;
        jsonl::read_before_with(&path, before_offset, limit, min_level, &mut PiParser)
    }

    fn search_in_session(
        &self,
        project: &str,
        session_id: &str,
        query: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let path = self
            .session_file(project, session_id, archive)
            .ok_or_else(|| {
                AppError::NotFound(format!("PiAgent session not found: {session_id}"))
            })?;
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(128 * 1024, file);
        let query = query.to_lowercase();
        let mut hits = Vec::new();
        let mut offset = 0_u64;
        loop {
            let mut line = String::new();
            let count = match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(count) => count,
                Err(_) => break,
            };
            let line_offset = offset;
            offset = offset.saturating_add(count as u64);
            if !line.to_lowercase().contains(&query) {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let timestamp = PiParser::timestamp(&value);
            for item in PiParser.push(&value, "debug") {
                let mut text = item.content_preview.clone();
                if let Some(tool_name) = &item.tool_name {
                    text.push(' ');
                    text.push_str(tool_name);
                }
                if let Some(input) = &item.tool_input {
                    text.push(' ');
                    text.push_str(&input.to_string());
                }
                if !text.to_lowercase().contains(&query) {
                    continue;
                }
                hits.push(SessionSearchHit {
                    byte_offset: line_offset,
                    snippet: crate::services::search::extract_snippet(&text, &query),
                    record_type: if item.tool_name.is_some() {
                        "tool".to_string()
                    } else {
                        item.record_type
                    },
                    timestamp: timestamp.clone(),
                });
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "code-dejavu-pi-{}-{}-{id}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn provider_with_fixture(lines: &[&str]) -> (TestDir, PiProvider, PathBuf) {
        let dir = TestDir::new();
        let sessions = dir.0.join(".pi/agent/sessions/--project--");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("2026-08-12_session-123.jsonl");
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut provider = PiProvider::for_host(Host::Native, &dir.0);
        provider.archive_root = dir.0.join("archives/pi");
        (dir, provider, path)
    }

    fn write_session(path: &Path, session_id: &str, prompt: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(
            path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":{session_id},\"timestamp\":\"2026-08-12T01:00:00Z\",\"cwd\":\"/project\"}}\n{{\"type\":\"message\",\"id\":\"user\",\"parentId\":null,\"timestamp\":\"2026-08-12T01:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":{prompt}}}}}\n",
                session_id = serde_json::to_string(session_id).unwrap(),
                prompt = serde_json::to_string(prompt).unwrap(),
            ),
        )
        .unwrap();
    }

    fn fixture_lines() -> Vec<&'static str> {
        vec![
            r#"{"type":"session","version":3,"id":"session-123","timestamp":"2026-08-12T01:00:00.000Z","cwd":"/project"}"#,
            r#"{"type":"thinking_level_change","id":"level1","parentId":null,"timestamp":"2026-08-12T01:00:01.000Z","thinkingLevel":"high"}"#,
            r#"{"type":"message","id":"user1","parentId":"level1","timestamp":"2026-08-12T01:00:02.000Z","message":{"role":"user","content":"Build the feature","timestamp":1786496402000}}"#,
            r#"{"type":"message","id":"assistant1","parentId":"user1","timestamp":"2026-08-12T01:00:03.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Inspect first"},{"type":"text","text":"I will inspect it."},{"type":"toolCall","id":"call-a","name":"read","arguments":{"path":"a.rs"}},{"type":"toolCall","id":"call-b","name":"read","arguments":{"path":"b.rs"}}],"provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":3,"cacheWrite":2,"totalTokens":20}}}"#,
            r#"{"type":"message","id":"result1","parentId":"assistant1","timestamp":"2026-08-12T01:00:04.000Z","message":{"role":"toolResult","toolCallId":"call-a","toolName":"read","content":[{"type":"text","text":"file contents"}],"isError":false}}"#,
            r#"{"type":"session_info","id":"info1","parentId":"result1","timestamp":"2026-08-12T01:00:05.000Z","name":"Named Pi session"}"#,
        ]
    }

    #[test]
    fn scans_pi_identity_title_models_and_usage() {
        let (_dir, provider, path) = provider_with_fixture(&fixture_lines());
        let meta = PiProvider::scan_session(&path).unwrap();
        assert_eq!(meta.session_id, "session-123");
        assert_eq!(meta.cwd, "/project");
        assert_eq!(meta.first_prompt.as_deref(), Some("Build the feature"));
        assert_eq!(meta.agent_title.as_deref(), Some("Named Pi session"));
        assert_eq!(meta.tokens.input_tokens, 10);
        assert_eq!(meta.tokens.output_tokens, 5);
        assert_eq!(meta.tokens.cache_tokens, 5);
        assert_eq!(meta.tokens.total_tokens, 20);
        assert!(meta.model_contexts.iter().any(|context| {
            context.provider.as_deref() == Some("anthropic")
                && context.model.as_deref() == Some("claude-sonnet-4-5")
                && context.thinking_level.as_deref() == Some("high")
        }));

        let sessions = provider.list_sessions(None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project_path, "/project");
    }

    #[test]
    fn last_activity_uses_user_and_assistant_messages_only() {
        let lines = [
            r#"{"type":"session","version":3,"id":"activity","timestamp":"2026-08-12T01:00:00Z","cwd":"/project"}"#,
            r#"{"type":"message","id":"user","parentId":null,"timestamp":"2026-08-12T01:00:01Z","message":{"role":"user","content":"hello","timestamp":1786496401000}}"#,
            r#"{"type":"message","id":"result","parentId":"user","timestamp":"2026-08-12T01:00:10Z","message":{"role":"toolResult","toolCallId":"call","toolName":"read","content":[{"type":"text","text":"done"}],"timestamp":1786496410000}}"#,
            r#"{"type":"session_info","id":"title","parentId":"result","timestamp":"2026-08-12T01:00:20Z","name":"renamed"}"#,
        ];
        let (_dir, _provider, path) = provider_with_fixture(&lines);
        let meta = PiProvider::scan_session(&path).unwrap();
        let user = serde_json::from_str::<Value>(lines[1]).unwrap();
        assert_eq!(meta.updated_at, message_ts(&user));
    }

    #[test]
    fn project_relative_session_dir_is_resolved_from_project() {
        let dir = TestDir::new();
        let project = dir.0.join("project");
        fs::create_dir_all(project.join(".pi")).unwrap();
        fs::write(
            project.join(".pi/settings.json"),
            r#"{"sessionDir":".pi/sessions"}"#,
        )
        .unwrap();
        let provider = PiProvider::for_host(Host::Native, &dir.0);
        assert_eq!(
            provider.project_session_root(&project.to_string_lossy()),
            Some(project.join(".pi/sessions"))
        );
    }

    #[test]
    fn discovered_project_session_dir_becomes_a_watched_data_root() {
        let dir = TestDir::new();
        let default_sessions = dir.0.join(".pi/agent/sessions/--project--");
        let project = dir.0.join("project");
        let custom_sessions = project.join(".pi/sessions");
        fs::create_dir_all(&default_sessions).unwrap();
        fs::create_dir_all(project.join(".pi")).unwrap();
        fs::write(
            project.join(".pi/settings.json"),
            r#"{"sessionDir":".pi/sessions"}"#,
        )
        .unwrap();
        fs::write(
            default_sessions.join("session.jsonl"),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"watch\",\"timestamp\":\"2026-08-12T01:00:00Z\",\"cwd\":{}}}\n",
                serde_json::to_string(&project.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
        let provider = PiProvider::for_host(Host::Native, &dir.0);
        assert_eq!(provider.list_sessions(None).unwrap().len(), 1);
        assert!(provider.data_roots().contains(&custom_sessions));
    }

    #[test]
    fn parser_preserves_block_order_and_parallel_tool_group() {
        let (_dir, provider, _path) = provider_with_fixture(&fixture_lines());
        let page = provider
            .session_detail("/project", "session-123", 0, 100, "tool", None)
            .unwrap();
        let assistant: Vec<_> = page
            .records
            .iter()
            .filter(|item| item.group_id.as_deref() == Some("assistant1"))
            .collect();
        assert_eq!(assistant.len(), 4);
        assert_eq!(assistant[0].record_type, "thinking");
        assert_eq!(assistant[1].content_preview, "I will inspect it.");
        assert_eq!(assistant[2].tool_use_id.as_deref(), Some("call-a"));
        assert_eq!(assistant[3].tool_use_id.as_deref(), Some("call-b"));
        let result = page
            .records
            .iter()
            .find(|item| item.record_type == "tool_result")
            .unwrap();
        assert_eq!(result.tool_use_id.as_deref(), Some("call-a"));
    }

    #[test]
    fn malformed_lines_do_not_hide_valid_entries_or_break_manifest() {
        let mut lines = fixture_lines();
        lines.insert(1, "not-json");
        let (_dir, provider, _path) = provider_with_fixture(&lines);
        assert_eq!(provider.list_sessions(None).unwrap().len(), 1);
        let manifest = provider.index_manifest();
        assert_eq!(manifest.len(), 1);
        assert!(manifest[0].version.contains(':'));
    }

    #[test]
    fn source_order_keeps_abandoned_tree_branches_visible() {
        let lines = [
            r#"{"type":"session","version":3,"id":"tree","timestamp":"2026-08-12T01:00:00Z","cwd":"/tree"}"#,
            r#"{"type":"message","id":"root","parentId":null,"timestamp":"2026-08-12T01:00:01Z","message":{"role":"user","content":"root"}}"#,
            r#"{"type":"message","id":"old-branch","parentId":"root","timestamp":"2026-08-12T01:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"old branch"}],"provider":"test","model":"one","usage":{}}}"#,
            r#"{"type":"message","id":"new-branch","parentId":"root","timestamp":"2026-08-12T01:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"new branch"}],"provider":"test","model":"one","usage":{}}}"#,
        ];
        let (_dir, provider, _path) = provider_with_fixture(&lines);
        let page = provider
            .session_detail("/tree", "tree", 0, 100, "content", None)
            .unwrap();
        let text: Vec<_> = page
            .records
            .iter()
            .map(|item| item.content_preview.as_str())
            .collect();
        assert_eq!(text, vec!["root", "old branch", "new branch"]);
    }

    #[test]
    fn resume_command_quotes_session_id() {
        let provider = PiProvider::for_host(Host::Native, Path::new("/tmp/pi-home"));
        assert_eq!(
            provider.resume_command("id with space", &["--print".to_string()]),
            Some("pi --session \"id with space\" --print".to_string())
        );
    }

    #[test]
    fn snapshot_archives_agent_data_preserves_auth_and_indexes_session() {
        let dir = TestDir::new();
        let agent_dir = dir.0.join(".pi/agent");
        write_session(
            &agent_dir.join("sessions/project/original.jsonl"),
            "archived-session",
            "archived prompt",
        );
        fs::write(agent_dir.join("settings.json"), "{}").unwrap();
        fs::write(agent_dir.join("models.json"), "{}").unwrap();
        fs::write(agent_dir.join("auth.json"), "current-login").unwrap();
        let mut provider = PiProvider::for_host(Host::Native, &dir.0);
        provider.archive_root = dir.0.join("archives/pi");

        let profile = provider.create_profile(Some("test".to_string())).unwrap();
        let archived_agent = provider.archive_root.join(&profile.name).join("pi");
        assert_eq!(
            fs::read_to_string(agent_dir.join("auth.json")).unwrap(),
            "current-login"
        );
        assert!(!agent_dir.join("sessions").exists());
        assert!(!agent_dir.join("settings.json").exists());
        assert!(!agent_dir.join("models.json").exists());
        assert!(!archived_agent.join("auth.json").exists());
        assert!(archived_agent.join("settings.json").exists());
        assert!(archived_agent.join("models.json").exists());

        let docs = provider.index_documents().docs;
        let archived = docs
            .iter()
            .find(|doc| doc.session_id == "archived-session")
            .unwrap();
        assert_eq!(
            archived.archive_name.as_deref(),
            Some(profile.name.as_str())
        );
        let incremental = provider.index_documents_for(&HashSet::from([archived.key.clone()]));
        assert_eq!(incremental.docs.len(), 1);
        assert_eq!(
            incremental.docs[0].archive_name.as_deref(),
            Some(profile.name.as_str())
        );
        let detail = provider
            .session_detail(
                "/project",
                "archived-session",
                0,
                20,
                "content",
                Some(&profile.name),
            )
            .unwrap();
        assert!(detail
            .records
            .iter()
            .any(|record| record.content_preview == "archived prompt"));
    }

    #[test]
    fn restore_keeps_snapshot_and_current_auth_and_creates_auto_backup() {
        let dir = TestDir::new();
        let agent_dir = dir.0.join(".pi/agent");
        write_session(
            &agent_dir.join("sessions/project/original.jsonl"),
            "original",
            "original prompt",
        );
        fs::write(agent_dir.join("auth.json"), "original-login").unwrap();
        let mut provider = PiProvider::for_host(Host::Native, &dir.0);
        provider.archive_root = dir.0.join("archives/pi");
        let profile = provider
            .create_profile(Some("original".to_string()))
            .unwrap();

        write_session(
            &agent_dir.join("sessions/project/current.jsonl"),
            "current",
            "current prompt",
        );
        fs::write(agent_dir.join("auth.json"), "current-login").unwrap();
        provider.restore_profile(&profile.name).unwrap();

        assert!(provider.archive_root.join(&profile.name).exists());
        assert_eq!(
            fs::read_to_string(agent_dir.join("auth.json")).unwrap(),
            "current-login"
        );
        assert!(provider
            .session_file("/project", "original", None)
            .is_some());
        assert!(provider
            .list_profiles()
            .unwrap()
            .iter()
            .any(|archive| archive.name.starts_with("auto-")));
    }

    #[test]
    fn snapshot_can_be_renamed_and_deleted() {
        let dir = TestDir::new();
        let agent_dir = dir.0.join(".pi/agent");
        write_session(
            &agent_dir.join("sessions/project/session.jsonl"),
            "rename-me",
            "prompt",
        );
        let mut provider = PiProvider::for_host(Host::Native, &dir.0);
        provider.archive_root = dir.0.join("archives/pi");
        let profile = provider.create_profile(None).unwrap();

        provider.rename_profile(&profile.name, "renamed").unwrap();
        assert!(!provider.archive_root.join(&profile.name).exists());
        assert!(provider.archive_root.join("renamed").exists());
        provider.delete_profile("renamed").unwrap();
        assert!(!provider.archive_root.join("renamed").exists());
    }

    #[test]
    fn wsl_snapshot_root_is_namespaced_by_host() {
        let native = PiProvider::for_host(Host::Native, Path::new("/tmp/native-home"));
        let wsl = PiProvider::for_host(
            Host::Wsl {
                distro: "Ubuntu".to_string(),
                user: None,
            },
            Path::new("/tmp/wsl-home"),
        );
        assert_ne!(native.archive_root, wsl.archive_root);
        assert!(wsl.archive_root.ends_with("archives/pi/wsl-Ubuntu"));
    }

    #[test]
    fn direct_bash_message_becomes_a_paired_call_and_result() {
        let value = serde_json::from_str::<Value>(
            r#"{"type":"message","id":"bash1","timestamp":"2026-08-12T01:00:00Z","message":{"role":"bashExecution","command":"pwd","output":"/project","exitCode":0,"cancelled":false,"truncated":false}}"#,
        )
        .unwrap();
        let records = PiParser.push(&value, "tool");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tool_use_id, records[1].tool_use_id);
        assert_eq!(records[1].content_preview, "/project");
    }

    #[test]
    fn summaries_are_visible_and_searchable_at_content_level() {
        let value = serde_json::from_str::<Value>(
            r#"{"type":"compaction","id":"compact","timestamp":"2026-08-12T01:00:00Z","summary":"important summary"}"#,
        )
        .unwrap();
        let records = PiParser.push(&value, "content");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, "meta");
        assert_eq!(records[0].content_preview, "important summary");
    }
}
