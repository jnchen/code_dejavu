use crate::error::AppError;
use crate::models::session::{
    push_model_context, SessionModelInfo, SessionRecord, SessionSummary, SubagentInfo, SubagentMeta,
};
use chrono::{DateTime, Local, Utc};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

fn to_local_time(ts: &str) -> String {
    if let Ok(utc) = ts.parse::<DateTime<Utc>>() {
        let local: DateTime<Local> = utc.into();
        local.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        ts.replace('T', " ").chars().take(19).collect()
    }
}

/// Metadata Claude persists inside the JSONL itself. A generated `summary` and an explicit
/// `custom-title` are agent-owned names, not the user's first prompt.
pub fn read_claude_session_native_meta(
    path: &Path,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<SessionModelInfo>,
) {
    let Ok(file) = File::open(path) else {
        return (None, None, None, Vec::new());
    };
    let reader = BufReader::with_capacity(64 * 1024, file);
    let mut generated_title = None;
    let mut custom_title = None;
    let mut created_at = None;
    let mut updated_at = None;
    let mut contexts = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let record_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if record_type == "summary" {
            generated_title = value
                .get("summary")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
                .or(generated_title);
        } else if record_type == "custom-title" {
            custom_title = value
                .get("customTitle")
                .or_else(|| value.get("title"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
                .or(custom_title);
        }

        let role = value
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if record_type == "user"
            || record_type == "assistant"
            || role == "user"
            || role == "assistant"
        {
            if let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()) {
                let ts = to_local_time(ts);
                if created_at.is_none() {
                    created_at = Some(ts.clone());
                }
                updated_at = Some(ts);
            }
        }
        if record_type == "assistant" || role == "assistant" {
            push_model_context(
                &mut contexts,
                None,
                value
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
                None,
            );
        }
    }
    (
        custom_title.or(generated_title),
        created_at,
        updated_at,
        contexts,
    )
}

pub fn read_claude_session_summary_fast(
    path: &Path,
    project_slug: &str,
    project_path: &str,
) -> Option<SessionSummary> {
    let meta = fs::metadata(path).ok()?;
    let session_id = path.file_stem()?.to_string_lossy().to_string();
    let file_size = meta.len();
    let format_file_time = |time: std::io::Result<std::time::SystemTime>| {
        time.ok().map(|time| {
            let local: DateTime<Local> = time.into();
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        })
    };
    let created_at = format_file_time(meta.created());
    let updated_at = format_file_time(meta.modified());

    let subagent_dir = path.parent()?.join(&session_id).join("subagents");
    let subagent_count = if subagent_dir.exists() {
        fs::read_dir(&subagent_dir)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
                    .count() as u32
            })
            .unwrap_or(0)
    } else {
        0
    };

    let (agent_title, native_created_at, native_updated_at, model_contexts) =
        read_claude_session_native_meta(path);
    let created_at = native_created_at.or(created_at);
    let updated_at = native_updated_at.or(updated_at);
    Some(SessionSummary {
        source: "claude".to_string(),
        session_id,
        project: project_slug.to_string(),
        project_path: project_path.to_string(),
        first_prompt: None,
        agent_title,
        created_at,
        timestamp: updated_at.clone(),
        updated_at,
        file_size_bytes: file_size,
        subagent_count,
        archive_name: None,
        model_contexts,
    })
}

pub fn read_claude_session_first_prompt(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::with_capacity(64 * 1024, file);

    for (i, line) in reader.lines().enumerate() {
        if i > 100 {
            break;
        }
        let line = line.ok()?;
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            if val.get("type").and_then(|t| t.as_str()) == Some("user") {
                if val.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true) {
                    continue;
                }
                if let Some(msg) = val.get("message") {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        return Some(content.chars().take(200).collect());
                    }
                }
            }
        }
    }
    None
}

pub fn search_claude_session(
    path: &Path,
    query: &str,
) -> Vec<crate::models::session::SessionSearchHit> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let query_lower = query.to_lowercase();
    let mut hits = Vec::new();
    let mut byte_pos: u64 = 0;

    loop {
        let mut line = String::new();
        let n = match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let current_offset = byte_pos;
        byte_pos += n as u64;
        let line = line.trim_end_matches(['\r', '\n']).to_string();

        let line_lower = line.to_lowercase();
        if !line_lower.contains(&query_lower) {
            continue;
        }

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            let rtype = RecordParser::normalize_type(&val);
            // Cover the same scopes as global search — conversation, thinking, and tool output.
            // Otherwise jumping to a tool/thinking hit from global search finds nothing in-session.
            let mut matched: Option<(String, String)> = None;
            if rtype == "user" || rtype == "assistant" {
                let text = extract_content_preview(&val, &rtype);
                if !text.trim_start().starts_with('<') && text.to_lowercase().contains(&query_lower)
                {
                    matched = Some((rtype.clone(), text));
                }
            }
            if matched.is_none() && rtype == "assistant" {
                if let Some(think) = extract_thinking(&val) {
                    if think.to_lowercase().contains(&query_lower) {
                        matched = Some(("thinking".to_string(), think));
                    }
                }
            }
            if matched.is_none() && (rtype == "tool_result" || val.get("toolUseResult").is_some()) {
                let text = extract_content_preview(&val, "tool_result");
                if !text.is_empty() && text.to_lowercase().contains(&query_lower) {
                    matched = Some(("tool_result".to_string(), text));
                }
            }
            if let Some((record_type, text)) = matched {
                let snippet = super::search::extract_snippet(&text, &query_lower);
                let timestamp = val
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(to_local_time);
                hits.push(crate::models::session::SessionSearchHit {
                    byte_offset: current_offset,
                    snippet,
                    record_type,
                    timestamp,
                });
            }
        }
    }

    hits
}

/// Stateful parser that converts raw JSONL records into display records.
///
/// Handles the quirks of Claude Code's transcript format in ONE place so the
/// forward (seekable) and backward (tail) readers can't diverge:
///   - subagent records have no top-level `type` (resolved via message.role)
///   - main-session streaming uses type="message" with stop_reason=null chunks
///   - tool_result is wrapped in a user message (content[].type == tool_result)
///   - one assistant turn streams as multiple chunks ->merged here
///
/// This remains Claude-specific; other providers plug their own parser into the shared pager.
struct RecordParser {
    pending_text: String,
    pending_ts: Option<String>,
}

impl RecordParser {
    fn new() -> Self {
        Self {
            pending_text: String::new(),
            pending_ts: None,
        }
    }

    fn normalize_type(val: &serde_json::Value) -> String {
        let mut record_type = val
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown")
            .to_string();
        // First resolve message/unknown to the message role...
        if record_type == "message" || record_type == "unknown" {
            if let Some(role) = val
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str())
            {
                record_type = role.to_string();
            }
        }
        // ...then detect tool_result (depends on record_type being "user")
        if record_type == "user" {
            if let Some(content) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                if content
                    .iter()
                    .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                {
                    record_type = "tool_result".to_string();
                }
            }
        }
        record_type
    }

    fn dominated(level: &str, min_level: &str) -> bool {
        match min_level {
            "content" => level != "content",
            "tool" => level == "debug",
            _ => false,
        }
    }

    /// Feed one parsed JSON line. Returns 0+ display records.
    fn push(&mut self, val: &serde_json::Value, min_level: &str) -> Vec<SessionRecord> {
        let record_type = Self::normalize_type(val);
        let mut out = Vec::new();

        if record_type == "assistant" {
            let stop_reason = val.get("message").and_then(|m| m.get("stop_reason"));
            let is_streaming =
                stop_reason.is_none() || stop_reason == Some(&serde_json::Value::Null);

            let chunk_text = extract_content_preview(val, &record_type);
            let (chunk_tool, chunk_id, chunk_input) = extract_tool_info(val, &record_type);
            let chunk_ts = val
                .get("timestamp")
                .and_then(|t| t.as_str())
                .map(to_local_time);

            // Extended-thinking block → its own record (rendered collapsed by the UI),
            // emitted before any text/tool blocks of the same turn.
            if let Some(thinking) = extract_thinking(val) {
                if !Self::dominated("content", min_level) {
                    out.push(SessionRecord {
                        record_type: "thinking".to_string(),
                        content_preview: thinking,
                        timestamp: chunk_ts.clone(),
                        tool_name: None,
                        tool_use_id: None,
                        tool_input: None,
                        diff: None,
                        level: "content".to_string(),
                        byte_offset: 0,
                        group_id: None,
                        result_meta: None,
                    });
                }
            }

            if !chunk_text.is_empty() {
                if self.pending_ts.is_none() {
                    self.pending_ts = chunk_ts.clone();
                }
                self.pending_text.push_str(&chunk_text);
            }

            // Chunk carries a tool_use ->emit it now (accumulated text prepended)
            if chunk_tool.is_some() {
                let lead_text = std::mem::take(&mut self.pending_text);
                let ts = self.pending_ts.take().or(chunk_ts);
                // 1. Emit the assistant's lead-in text ("Let me check ...") as its own
                //    conversation bubble — it belongs to the chat, not buried in the tool card.
                if !lead_text.is_empty() && !Self::dominated("content", min_level) {
                    out.push(SessionRecord {
                        record_type: "assistant".to_string(),
                        content_preview: lead_text,
                        timestamp: ts.clone(),
                        tool_name: None,
                        tool_use_id: None,
                        tool_input: None,
                        diff: None,
                        level: "content".to_string(),
                        byte_offset: 0,
                        group_id: None,
                        result_meta: None,
                    });
                }
                // 2. Emit the tool call itself (no bundled text).
                let level = classify_level(val, &record_type, false, true);
                if !Self::dominated(&level, min_level) {
                    let diff = extract_diff(val);
                    out.push(SessionRecord {
                        record_type,
                        content_preview: String::new(),
                        timestamp: ts,
                        tool_name: chunk_tool,
                        tool_use_id: chunk_id,
                        tool_input: chunk_input,
                        diff,
                        level,
                        byte_offset: 0,
                        group_id: None,
                        result_meta: None,
                    });
                }
                return out;
            }

            // No tool, still streaming ->keep accumulating
            if is_streaming {
                return out;
            }

            // Final chunk, text only ->emit merged text record
            let content_preview = std::mem::take(&mut self.pending_text);
            if content_preview.is_empty() {
                return out;
            }
            let timestamp = self.pending_ts.take().or(chunk_ts);
            let level = classify_level(val, &record_type, true, false);
            if !Self::dominated(&level, min_level) {
                out.push(SessionRecord {
                    record_type,
                    content_preview,
                    timestamp,
                    tool_name: None,
                    tool_use_id: None,
                    tool_input: None,
                    diff: None,
                    level,
                    byte_offset: 0,
                    group_id: None,
                    result_meta: None,
                });
            }
            return out;
        }

        // Attachment / session-metadata records.
        if record_type == "attachment" || is_metadata_type(&record_type) {
            let timestamp = val
                .get("timestamp")
                .and_then(|t| t.as_str())
                .map(to_local_time);
            // Per-tool hook (Pre/PostToolUse) → a "hook" record carrying its tool_use_id, so the
            // UI can hang it on the matching call card instead of breaking the call→result flow.
            if let Some((preview, tuid)) = describe_tool_hook(val) {
                if !Self::dominated("debug", min_level) {
                    out.push(SessionRecord {
                        record_type: "hook".to_string(),
                        content_preview: preview,
                        timestamp,
                        tool_name: None,
                        tool_use_id: tuid,
                        tool_input: None,
                        diff: None,
                        level: "debug".to_string(),
                        byte_offset: 0,
                        group_id: None,
                        result_meta: None,
                    });
                }
                return out;
            }
            // Friendly-label the meaningful few. At DEBUG nothing is hidden: anything without a
            // friendly label is shown RAW (collapsed) so the parser never silently decides for you.
            return match describe_meta(val, &record_type) {
                Some(preview) => {
                    if !Self::dominated("debug", min_level) {
                        out.push(SessionRecord {
                            record_type: "meta".to_string(),
                            content_preview: preview,
                            timestamp,
                            tool_name: None,
                            tool_use_id: None,
                            tool_input: None,
                            diff: None,
                            level: "debug".to_string(),
                            byte_offset: 0,
                            group_id: None,
                            result_meta: None,
                        });
                    }
                    out
                }
                None => {
                    if min_level == "debug" {
                        out.push(SessionRecord {
                            record_type: "meta".to_string(),
                            content_preview: raw_debug_preview(val, &record_type),
                            timestamp,
                            tool_name: None,
                            tool_use_id: None,
                            tool_input: None,
                            diff: None,
                            level: "debug".to_string(),
                            byte_offset: 0,
                            group_id: None,
                            result_meta: None,
                        });
                    }
                    out
                }
            };
        }

        // Non-assistant record (user / tool_result / system / ...)
        let content_preview = extract_content_preview(val, &record_type);
        let (tool_name, tool_use_id, tool_input) = extract_tool_info(val, &record_type);
        let level = classify_level(
            val,
            &record_type,
            !content_preview.is_empty(),
            tool_name.is_some(),
        );
        if Self::dominated(&level, min_level) {
            return out;
        }
        let timestamp = val
            .get("timestamp")
            .and_then(|t| t.as_str())
            .map(to_local_time);
        let diff = extract_diff(val);
        let result_meta = extract_result_meta(val, &record_type);
        out.push(SessionRecord {
            record_type,
            content_preview,
            timestamp,
            tool_name,
            tool_use_id,
            tool_input,
            diff,
            level,
            byte_offset: 0,
            group_id: None,
            result_meta,
        });
        out
    }

    /// Flush trailing accumulated text at EOF (stream cut off mid-turn).
    fn flush(&mut self, min_level: &str) -> Vec<SessionRecord> {
        let content_preview = std::mem::take(&mut self.pending_text);
        if content_preview.is_empty() || Self::dominated("content", min_level) {
            return Vec::new();
        }
        vec![SessionRecord {
            record_type: "assistant".to_string(),
            content_preview,
            timestamp: self.pending_ts.take(),
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            diff: None,
            level: "content".to_string(),
            byte_offset: 0,
            group_id: None,
            result_meta: None,
        }]
    }
}

/// Claude's record stream plugged into the generic pager. The expensive seek/paginate/cache
/// machinery in `read_seekable_cached` is shared; only these four hooks are Claude-specific.
impl crate::agents::LineParser for RecordParser {
    fn reset(&mut self) {
        *self = RecordParser::new();
    }
    fn push(&mut self, val: &serde_json::Value, min_level: &str) -> Vec<SessionRecord> {
        RecordParser::push(self, val, min_level)
    }
    fn flush(&mut self, min_level: &str) -> Vec<SessionRecord> {
        RecordParser::flush(self, min_level)
    }
    fn group_of(&self, val: &serde_json::Value) -> Option<String> {
        // Records sharing a message.id were one assistant turn → a parallel tool-call batch.
        val.get("message")
            .and_then(|m| m.get("id"))
            .and_then(|i| i.as_str())
            .map(String::from)
    }
    fn skippable(&self, line: &str, min_level: &str) -> bool {
        min_level == "content" && skippable_at_content(line)
    }
}

/// Cheap raw-line check: at the "content" view, tool calls/results with no displayable
/// text are dropped anyway — so skip the (potentially multi-MB) serde parse entirely.
/// Only returns true when the line is certainly non-displayable at content level, so no
/// conversation text is ever lost. This is the hot-path optimization for large sessions.
#[inline]
fn skippable_at_content(line: &str) -> bool {
    // Tool results are always dropped at the content view — and they're the big lines
    // (file reads, command output, web fetches). Skip them regardless of whether their
    // content is stored as text blocks (`[{type:"text"}]`) — the marker is the result field.
    if line.contains("\"toolUseResult\"") || line.contains("\"type\":\"tool_result\"") {
        return true;
    }
    // Attachment records (context dumps, command stdout) are debug-level, never shown here.
    if line.contains("\"type\":\"attachment\"") {
        return true;
    }
    // A tool_use call with NO accompanying text block is also dropped. If a text block is
    // present (lead-in or mixed turn), we must parse it so the conversation text survives.
    if line.contains("\"type\":\"tool_use\"") && !line.contains("\"type\":\"text\"") {
        return true;
    }
    false
}

// ---------- Page LRU cache ----------
// Transcripts are APPEND-ONLY, so any FULL interior page (has_more==true) is immutable — its
// bytes never change once written. We cache those by (path, level, offset, limit) with NO mtime,
// so even the live, still-growing session serves its interior pages from cache. Only the final
// (partial, has_more==false) page is left uncached, since appends extend it.
struct PageCache {
    map: std::collections::HashMap<String, crate::models::session::PaginatedRecords>,
    order: std::collections::VecDeque<String>,
    cap: usize,
}
static PAGE_CACHE: std::sync::OnceLock<std::sync::Mutex<PageCache>> = std::sync::OnceLock::new();
fn page_cache() -> &'static std::sync::Mutex<PageCache> {
    PAGE_CACHE.get_or_init(|| {
        std::sync::Mutex::new(PageCache {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap: 100,
        })
    })
}
fn page_cache_key(path: &Path, byte_offset: u64, limit: u32, min_level: &str) -> String {
    format!("{}|{}|{}|{}", path.display(), min_level, byte_offset, limit)
}
fn page_cache_get(key: &str) -> Option<crate::models::session::PaginatedRecords> {
    let mut c = page_cache().lock().ok()?;
    let hit = c.map.get(key).cloned();
    if hit.is_some() {
        if let Some(pos) = c.order.iter().position(|k| k == key) {
            c.order.remove(pos);
        }
        c.order.push_back(key.to_string());
    }
    hit
}
fn page_cache_put(key: String, val: crate::models::session::PaginatedRecords) {
    if let Ok(mut c) = page_cache().lock() {
        if c.map.contains_key(&key) {
            return;
        }
        c.map.insert(key.clone(), val);
        c.order.push_back(key);
        while c.order.len() > c.cap {
            if let Some(old) = c.order.pop_front() {
                c.map.remove(&old);
            }
        }
    }
}

/// Claude entry point: read a page with Claude's parser. Thin wrapper over the generic pager.
pub fn read_claude_records_seekable(
    path: &Path,
    byte_offset: u64,
    limit: u32,
    min_level: &str,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    read_seekable_cached(
        path,
        byte_offset,
        limit,
        min_level,
        &mut RecordParser::new(),
    )
}

/// Generic, agent-agnostic page reader: seek + paginate + byte-offset anchor + append-only LRU
/// cache. Any agent reuses all of this by passing its own [`crate::agents::LineParser`].
pub fn read_seekable_cached(
    path: &Path,
    byte_offset: u64,
    limit: u32,
    min_level: &str,
    parser: &mut dyn crate::agents::LineParser,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    let key = page_cache_key(path, byte_offset, limit, min_level);
    if let Some(hit) = page_cache_get(&key) {
        return Ok(hit);
    }
    let res = read_seekable_inner(path, byte_offset, limit, min_level, parser)?;
    // Only cache full interior pages — they're immutable (append-only). The final page grows.
    if res.has_more {
        page_cache_put(key, res.clone());
    }
    Ok(res)
}

fn read_seekable_inner(
    path: &Path,
    byte_offset: u64,
    limit: u32,
    min_level: &str,
    parser: &mut dyn crate::agents::LineParser,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    use std::io::{Read as _, Seek};

    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let requested_offset = byte_offset.min(file_size);
    let mut bytes_read = requested_offset;

    // A caller-provided/search cursor may point into a UTF-8 code point or the middle of a JSONL
    // record. Align with the next complete source line using raw bytes, so the cursor always moves
    // forwards and invalid UTF-8 can never create an empty-page retry loop.
    let aligned = if requested_offset == 0 {
        true
    } else {
        file.seek(std::io::SeekFrom::Start(requested_offset - 1))?;
        let mut previous = [0_u8; 1];
        file.read_exact(&mut previous)?;
        previous[0] == b'\n'
    };
    file.seek(std::io::SeekFrom::Start(requested_offset))?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    if !aligned && requested_offset < file_size {
        let mut partial = Vec::new();
        bytes_read += reader.read_until(b'\n', &mut partial)? as u64;
    }

    let start_byte_offset = bytes_read;
    let mut records = Vec::new();
    let mut last_line_start = start_byte_offset;

    if limit == 0 {
        return Ok(crate::models::session::PaginatedRecords {
            records,
            start_byte_offset,
            next_byte_offset: start_byte_offset,
            has_earlier: start_byte_offset > 0,
            has_more: start_byte_offset < file_size,
        });
    }

    loop {
        let mut line = Vec::new();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        let line_start = bytes_read;
        bytes_read += n as u64;
        last_line_start = line_start;

        // A source line is the smallest cursor-addressable unit. Add every record emitted by it
        // atomically, even if that makes the page slightly larger than `limit`; otherwise advancing
        // past the line would permanently drop the remaining records from that same line.
        let emitted = parse_source_line(&line, line_start, min_level, parser);
        records.extend(emitted);
        if records.len() >= limit as usize {
            return Ok(crate::models::session::PaginatedRecords {
                records,
                start_byte_offset,
                next_byte_offset: bytes_read,
                has_earlier: start_byte_offset > 0,
                has_more: bytes_read < file_size,
            });
        }
    }

    let mut flushed = parser.flush(min_level);
    for rec in &mut flushed {
        rec.byte_offset = last_line_start;
    }
    records.extend(flushed);

    Ok(crate::models::session::PaginatedRecords {
        records,
        start_byte_offset,
        next_byte_offset: bytes_read,
        has_earlier: start_byte_offset > 0,
        has_more: bytes_read < file_size,
    })
}

fn trim_line_end(mut line: &[u8]) -> &[u8] {
    while line.last().is_some_and(|b| matches!(b, b'\r' | b'\n')) {
        line = &line[..line.len() - 1];
    }
    line
}

fn parse_source_line(
    line: &[u8],
    line_start: u64,
    min_level: &str,
    parser: &mut dyn crate::agents::LineParser,
) -> Vec<SessionRecord> {
    let line = trim_line_end(line);
    if line.is_empty() {
        return Vec::new();
    }
    // Preserve the content-view fast path when the complete line is valid UTF-8. Invalid or
    // concurrently half-written lines are simply not valid JSON records and are skipped below.
    if std::str::from_utf8(line)
        .ok()
        .is_some_and(|text| parser.skippable(text, min_level))
    {
        return Vec::new();
    }
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let group = parser.group_of(&val);
    let mut records = parser.push(&val, min_level);
    for rec in &mut records {
        rec.byte_offset = line_start;
        if rec.group_id.is_none() {
            rec.group_id = group.clone();
        }
    }
    records
}

/// Claude entry point: tail with Claude's parser. Thin wrapper over the generic tail reader.
pub fn read_claude_records_tail(
    path: &Path,
    limit: u32,
    min_level: &str,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    read_tail_with(path, limit, min_level, &mut RecordParser::new())
}

/// Claude entry point for reverse paging. `before_offset` is an exclusive source-byte cursor.
pub fn read_claude_records_before(
    path: &Path,
    before_offset: u64,
    limit: u32,
    min_level: &str,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    read_before_with(
        path,
        before_offset,
        limit,
        min_level,
        &mut RecordParser::new(),
    )
}

/// Generic, agent-agnostic tail reader: grab the last records of a session at `min_level`.
pub fn read_tail_with(
    path: &Path,
    limit: u32,
    min_level: &str,
    parser: &mut dyn crate::agents::LineParser,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    // `u64::MAX` is clamped to the same metadata snapshot inside `read_before_with`, so an append
    // between separate metadata/read calls cannot accidentally turn a tail page into an interior
    // page. Tail and reverse paging therefore share one cursor implementation.
    read_before_with(path, u64::MAX, limit, min_level, parser)
}

#[derive(Debug)]
struct SourceBatch {
    start: u64,
    records: Vec<SessionRecord>,
}

/// Generic reverse pager. It reads an exponentially growing raw-byte window ending strictly at
/// `before_offset`, aligns both sides to JSONL boundaries, and parses records in chronological
/// order. One non-empty source batch is retained as parser warm-up before the selected suffix; if
/// that is not possible the window grows until BOF. This keeps normal tail reads bounded while
/// ensuring the returned cursor never points at a coarse chunk boundary that would skip records.
pub fn read_before_with(
    path: &Path,
    before_offset: u64,
    limit: u32,
    min_level: &str,
    parser: &mut dyn crate::agents::LineParser,
) -> Result<crate::models::session::PaginatedRecords, AppError> {
    use std::io::{Read as _, Seek};

    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let requested = before_offset.min(file_size);
    let is_tail = before_offset == u64::MAX;
    let boundary = if is_tail || requested == file_size {
        file_size
    } else {
        align_before_boundary(&mut file, requested)?
    };

    if limit == 0 || boundary == 0 {
        return Ok(crate::models::session::PaginatedRecords {
            records: Vec::new(),
            start_byte_offset: boundary,
            next_byte_offset: boundary,
            has_earlier: false,
            has_more: boundary < file_size,
        });
    }

    let target = limit as usize;
    let mut window_size = ((limit as u64).saturating_mul(5_000)).max(64 * 1024);
    window_size = window_size.min(boundary);

    loop {
        let raw_start = boundary.saturating_sub(window_size);
        file.seek(std::io::SeekFrom::Start(raw_start))?;
        let mut raw = vec![0_u8; (boundary - raw_start) as usize];
        file.read_exact(&mut raw)?;

        let (window_start, complete) = if raw_start == 0 {
            (0, raw.as_slice())
        } else if let Some(newline) = raw.iter().position(|b| *b == b'\n') {
            let first_complete = newline + 1;
            (raw_start + first_complete as u64, &raw[first_complete..])
        } else {
            // The chunk began inside a very large JSONL record. Grow until a full line or BOF is
            // available; decoding is never attempted on this partial byte sequence.
            if raw_start == 0 {
                unreachable!();
            }
            window_size = window_size.saturating_mul(2).min(boundary);
            continue;
        };

        parser.reset();
        let (mut batches, last_line_start) =
            parse_source_window(complete, window_start, min_level, parser);
        if is_tail {
            let flush_offset = last_line_start.unwrap_or(window_start);
            let mut flushed = parser.flush(min_level);
            for rec in &mut flushed {
                rec.byte_offset = flush_offset;
            }
            push_source_batch(&mut batches, flush_offset, flushed);
        }

        let (selected_start, selected_count) = suffix_start(&batches, target);
        let at_bof = raw_start == 0;
        let has_warmup_batch = selected_start > 0;
        if at_bof || (selected_count >= target && has_warmup_batch) {
            let has_earlier = selected_start > 0;
            let start_byte_offset = batches
                .get(selected_start)
                .map(|batch| batch.start)
                .unwrap_or(0);
            let records = batches
                .into_iter()
                .skip(selected_start)
                .flat_map(|batch| batch.records)
                .collect();
            return Ok(crate::models::session::PaginatedRecords {
                records,
                start_byte_offset,
                next_byte_offset: boundary,
                has_earlier,
                has_more: boundary < file_size,
            });
        }

        let grown = window_size.saturating_mul(2).min(boundary);
        if grown == window_size {
            // Defensive fallback for an unexpected zero-growth corner; the next iteration at BOF
            // is the authoritative result.
            window_size = boundary;
        } else {
            window_size = grown;
        }
    }
}

/// Session files can be replaced in-place by profile restore/create operations, violating the
/// append-only assumption used by the interior-page cache. Clear it after those mutations.
pub fn clear_page_cache() {
    if let Ok(mut cache) = page_cache().lock() {
        cache.map.clear();
        cache.order.clear();
    }
}

/// If a reverse cursor lands in the middle of a line/code point, exclude that partial source line
/// by moving the exclusive boundary to its byte-aligned beginning.
fn align_before_boundary(file: &mut File, requested: u64) -> Result<u64, AppError> {
    use std::io::{Read as _, Seek};

    if requested == 0 {
        return Ok(0);
    }
    file.seek(std::io::SeekFrom::Start(requested - 1))?;
    let mut previous = [0_u8; 1];
    file.read_exact(&mut previous)?;
    if previous[0] == b'\n' {
        return Ok(requested);
    }

    let mut search_end = requested;
    while search_end > 0 {
        let search_start = search_end.saturating_sub(8 * 1024);
        file.seek(std::io::SeekFrom::Start(search_start))?;
        let mut bytes = vec![0_u8; (search_end - search_start) as usize];
        file.read_exact(&mut bytes)?;
        if let Some(pos) = bytes.iter().rposition(|b| *b == b'\n') {
            return Ok(search_start + pos as u64 + 1);
        }
        search_end = search_start;
    }
    Ok(0)
}

fn parse_source_window(
    bytes: &[u8],
    start_offset: u64,
    min_level: &str,
    parser: &mut dyn crate::agents::LineParser,
) -> (Vec<SourceBatch>, Option<u64>) {
    let mut batches = Vec::new();
    let mut consumed = 0_usize;
    let mut last_line_start = None;
    while consumed < bytes.len() {
        let line_len = bytes[consumed..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|pos| pos + 1)
            .unwrap_or(bytes.len() - consumed);
        let line_start = start_offset + consumed as u64;
        last_line_start = Some(line_start);
        let records = parse_source_line(
            &bytes[consumed..consumed + line_len],
            line_start,
            min_level,
            parser,
        );
        push_source_batch(&mut batches, line_start, records);
        consumed += line_len;
    }
    (batches, last_line_start)
}

fn push_source_batch(batches: &mut Vec<SourceBatch>, start: u64, records: Vec<SessionRecord>) {
    if records.is_empty() {
        return;
    }
    if let Some(last) = batches.last_mut().filter(|batch| batch.start == start) {
        last.records.extend(records);
    } else {
        batches.push(SourceBatch { start, records });
    }
}

fn suffix_start(batches: &[SourceBatch], target: usize) -> (usize, usize) {
    let mut start = batches.len();
    let mut count = 0;
    while start > 0 && count < target {
        start -= 1;
        count += batches[start].records.len();
    }
    (start, count)
}

/// True if this assistant record's tool_use is a user-facing interactive prompt.
fn assistant_tool_is_interactive(val: &serde_json::Value) -> bool {
    val.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter().any(|item| {
                item.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && item.get("name").and_then(|n| n.as_str()) == Some("AskUserQuestion")
            })
        })
        .unwrap_or(false)
}

fn classify_level(
    val: &serde_json::Value,
    record_type: &str,
    has_content: bool,
    has_tool: bool,
) -> String {
    match record_type {
        "user" => {
            if !has_content {
                return "debug".to_string();
            }
            let is_system_noise = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .is_some_and(|s| {
                    let t = s.trim_start();
                    t.starts_with("<local-command")
                        || t.starts_with("<command-name>")
                        || t.starts_with("<command-message>")
                        || t.starts_with("<local-command-stdout>")
                        || t.starts_with("<local-command-caveat>")
                        || t.starts_with("<task-notification")
                        || t.starts_with("<system-reminder")
                        || t.starts_with("<tool-")
                        || t.starts_with("<")
                });
            if is_system_noise {
                "debug".to_string()
            } else {
                "content".to_string()
            }
        }
        "assistant" => {
            if has_content {
                "content".to_string()
            } else if has_tool {
                // User-facing interactive tools (AskUserQuestion) are part of the
                // conversation, not debug-only tooling --keep them at content level.
                if assistant_tool_is_interactive(val) {
                    "content".to_string()
                } else {
                    "tool".to_string()
                }
            } else {
                "debug".to_string()
            }
        }
        "tool_result" => {
            // Interactive answers (AskUserQuestion) belong in the conversation view.
            if val
                .get("toolUseResult")
                .and_then(|r| r.get("answers"))
                .is_some()
            {
                "content".to_string()
            } else {
                "tool".to_string()
            }
        }
        "system" => {
            let subtype = val.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            if subtype == "compact_boundary" {
                "content".to_string()
            } else {
                "debug".to_string()
            }
        }
        _ => "debug".to_string(),
    }
}

pub fn list_claude_subagents(session_dir: &Path) -> Result<Vec<SubagentInfo>, AppError> {
    let subagent_dir = session_dir.join("subagents");
    if !subagent_dir.exists() {
        return Ok(Vec::new());
    }

    let mut agents = Vec::new();
    for entry in fs::read_dir(&subagent_dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<SubagentMeta>(&content) {
                    let agent_id = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                        .trim_end_matches(".meta")
                        .to_string();

                    let jsonl_path = subagent_dir.join(format!("{}.jsonl", agent_id));
                    let record_count = if jsonl_path.exists() {
                        let size = fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
                        (size / 400).max(1) as u32
                    } else {
                        0
                    };

                    agents.push(SubagentInfo {
                        agent_id,
                        agent_type: meta.agent_type,
                        description: meta.description,
                        tool_use_id: meta.tool_use_id,
                        record_count,
                    });
                }
            }
        }
    }

    Ok(agents)
}

/// Pull text out of a tool_result `content` value, which may be a plain string or an
/// array of content blocks ({type:"text", text}, or nested {content}). Returns None if
/// there's no usable text (e.g. only tool_reference / image blocks).
fn text_from_content(c: Option<&serde_json::Value>) -> Option<String> {
    let c = c?;
    if let Some(s) = c.as_str() {
        return if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }
    if let Some(arr) = c.as_array() {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(t.to_string());
            } else if let Some(t) = item.get("content").and_then(|t| t.as_str()) {
                parts.push(t.to_string());
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    None
}

/// Top-level session-state record types (latest-value-wins pointers, no timestamp).
fn is_metadata_type(record_type: &str) -> bool {
    matches!(
        record_type,
        "last-prompt"
            | "ai-title"
            | "mode"
            | "permission-mode"
            | "agent-name"
            | "file-history-snapshot"
    )
}

/// A per-tool hook (any hook attachment carrying a toolUseID, e.g. PreToolUse / PostToolUse /
/// PostToolUseFailure) → (display text starting with `🪝 <event>`, the tool_use_id it belongs to).
/// These hang on the matching call card. async_hook_response (background no-ops) is left to
/// describe_meta, which hides it.
fn describe_tool_hook(val: &serde_json::Value) -> Option<(String, Option<String>)> {
    let a = val.get("attachment")?;
    let t = a.get("type").and_then(|x| x.as_str())?;
    if !matches!(
        t,
        "hook_success" | "hook_cancelled" | "hook_non_blocking_error"
    ) {
        return None;
    }
    let event = a.get("hookEvent").and_then(|x| x.as_str()).unwrap_or("");
    // ONLY genuine per-tool hooks hang on a call card. Session-level hooks (SessionStart, Stop,
    // SessionEnd, UserPromptSubmit, …) ALSO carry a toolUseID, but it's a synthetic hook id that
    // matches no real tool_use — claiming them here makes the frontend (which hides standalone
    // hooks) drop them entirely. Route everything else to describe_meta so it stays visible.
    if !matches!(event, "PreToolUse" | "PostToolUse" | "PostToolUseFailure") {
        return None;
    }
    let tuid = a.get("toolUseID").and_then(|x| x.as_str())?.to_string();
    let name = a.get("hookName").and_then(|x| x.as_str()).unwrap_or("");
    let tag = match t {
        "hook_cancelled" => " (取消)",
        "hook_non_blocking_error" => " (错误)",
        _ => "",
    };
    // Keep `🪝 <event>` first so the UI can split Pre- vs Post- by prefix; append name if distinct.
    let head = if name.is_empty() || name.starts_with(event) {
        format!("🪝 {}{}", if name.is_empty() { event } else { name }, tag)
    } else {
        format!("🪝 {}{} · {}", event, tag, name)
    };
    let out = a
        .get("stdout")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    let preview = if out.is_empty() {
        head
    } else {
        format!("{}\n{}", head, out)
    };
    Some((preview, Some(tuid)))
}

/// DEBUG fallback for records `describe_meta` has no friendly label for: show them RAW so the
/// view is exhaustive. First line = a compact type label (what MetaBlock shows collapsed); the
/// full pretty-printed record follows (expand to read it all — never truncated).
fn raw_debug_preview(val: &serde_json::Value, record_type: &str) -> String {
    let label = if record_type == "attachment" {
        let a = val.get("attachment");
        let sub = a
            .and_then(|a| a.get("type"))
            .and_then(|x| x.as_str())
            .unwrap_or("attachment");
        match a.and_then(|a| a.get("hookEvent")).and_then(|x| x.as_str()) {
            Some(ev) => format!("ⓘ attachment · {} · {}", sub, ev),
            None => format!("ⓘ attachment · {}", sub),
        }
    } else {
        format!("ⓘ {}", record_type)
    };
    let body = serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string());
    format!("{}\n{}", label, body)
}

/// Decide how to display an attachment / session-metadata record:
/// `None` = hide (pure internal bookkeeping), `Some(text)` = show with this friendly label.
fn describe_meta(val: &serde_json::Value, record_type: &str) -> Option<String> {
    let s =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());

    // Top-level state pointers
    match record_type {
        "ai-title" => return s(val, "aiTitle").map(|t| format!("📝 会话标题: {}", t)),
        "agent-name" => return s(val, "agentName").map(|t| format!("🤖 当前 Agent: {}", t)),
        "last-prompt" | "mode" | "permission-mode" | "file-history-snapshot" => return None,
        _ => {}
    }

    // Attachment subtypes
    if record_type == "attachment" {
        let a = val.get("attachment")?;
        let sub = a.get("type").and_then(|x| x.as_str()).unwrap_or("");
        return match sub {
            "queued_command" => s(a, "prompt").map(|p| format!("⌨ 排队指令: {}", p)),
            "plan_mode" => Some("📋 进入计划模式".to_string()),
            "plan_mode_exit" => Some("📋 退出计划模式".to_string()),
            "plan_mode_reentry" => Some("📋 重新进入计划模式".to_string()),
            "edited_text_file" => s(a, "filename").map(|f| format!("✏️ 编辑文件: {}", f)),
            // A memory/rules file auto-loaded into context. The file text lives at content.content.
            "nested_memory" => {
                let display = a
                    .get("displayPath")
                    .and_then(|x| x.as_str())
                    .or_else(|| a.get("path").and_then(|x| x.as_str()))
                    .unwrap_or("");
                let text = a
                    .get("content")
                    .and_then(|c| c.get("content"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if text.is_empty() {
                    Some(format!("📄 加载记忆/规则: {}", display))
                } else {
                    Some(format!("📄 加载记忆/规则: {}\n\n{}", display, text))
                }
            }
            "task_reminder" => a
                .get("itemCount")
                .map(|c| format!("✅ 任务提醒（{} 项）", c)),
            "skill_listing" => {
                if a.get("isInitial")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false)
                {
                    a.get("skillCount")
                        .map(|c| format!("⚡ 加载技能（{} 个）", c))
                } else {
                    None
                }
            }
            // First line = compact marker (collapsed view); full stdout below (expand to read).
            "hook_success" => {
                let event = a
                    .get("hookEvent")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Hook");
                let out = s(a, "stdout").unwrap_or_default();
                let out = out.trim();
                Some(if out.is_empty() {
                    format!("🪝 {}", event)
                } else {
                    format!("🪝 {}\n{}", event, out)
                })
            }
            // Pure internal bookkeeping — hide.
            "async_hook_response"
            | "hook_additional_context"
            | "deferred_tools_delta"
            | "mcp_instructions_delta"
            | "command_permissions"
            | "ultra_effort_enter" => None,
            // Unknown attachment: keep its content if it carries any, else hide.
            other => s(a, "content").filter(|c| !c.is_empty()).or_else(|| {
                if other.is_empty() {
                    None
                } else {
                    Some(format!("[附件: {}]", other))
                }
            }),
        };
    }

    None
}

/// Extract extended-thinking text from an assistant record's content blocks.
fn extract_thinking(val: &serde_json::Value) -> Option<String> {
    let arr = val.get("message")?.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for item in arr {
        if item.get("type").and_then(|t| t.as_str()) == Some("thinking") {
            if let Some(t) = item.get("thinking").and_then(|t| t.as_str()) {
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn extract_content_preview(val: &serde_json::Value, record_type: &str) -> String {
    match record_type {
        "user" => {
            if let Some(msg) = val.get("message") {
                if let Some(content) = msg.get("content") {
                    if let Some(s) = content.as_str() {
                        return s.to_string();
                    }
                    if let Some(arr) = content.as_array() {
                        for item in arr {
                            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    return text.to_string();
                                }
                            }
                        }
                    }
                }
            }
            String::new()
        }
        "assistant" => {
            if let Some(msg) = val.get("message") {
                if let Some(content) = msg.get("content") {
                    if let Some(arr) = content.as_array() {
                        let mut parts = Vec::new();
                        for item in arr {
                            match item.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                        parts.push(text.to_string());
                                    }
                                }
                                Some("tool_use") => {}
                                _ => {}
                            }
                        }
                        return parts.join(" ");
                    }
                    if let Some(s) = content.as_str() {
                        return s.to_string();
                    }
                }
            }
            String::new()
        }
        "tool_result" => {
            // Prefer the tool_result content block in the user message (what the model saw).
            // `content` may be a plain string OR an array of {type:text|...} blocks.
            if let Some(msg) = val.get("message") {
                if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for item in arr {
                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                            if let Some(s) = text_from_content(item.get("content")) {
                                return s;
                            }
                        }
                    }
                }
                if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
            // Fallback: the structured toolUseResult payload. Different tools use different
            // text fields (WebFetch=result, Bash=stdout, etc.); try the common ones.
            if let Some(tur) = val.get("toolUseResult") {
                if let Some(s) = tur.as_str() {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
                for key in ["content", "result", "stdout", "output", "text"] {
                    if let Some(s) = tur.get(key).and_then(|c| c.as_str()) {
                        if !s.is_empty() {
                            return s.to_string();
                        }
                    }
                }
                if let Some(s) = text_from_content(tur.get("content")) {
                    return s;
                }
            }
            String::new()
        }
        "system" => val
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        "attachment" => {
            let att = val.get("attachment");
            let att_type = att
                .and_then(|a| a.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let content = att
                .and_then(|a| a.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if content.is_empty() {
                att_type.to_string()
            } else {
                format!("[{}] {}", att_type, content)
            }
        }
        _ => String::new(),
    }
}

fn extract_diff(val: &serde_json::Value) -> Option<crate::models::session::DiffInfo> {
    let result = val.get("toolUseResult")?;
    let file_path = result.get("filePath").and_then(|f| f.as_str())?.to_string();
    let patches = result.get("structuredPatch").and_then(|p| p.as_array())?;
    if patches.is_empty() {
        return None;
    }

    let hunks: Vec<crate::models::session::DiffHunk> = patches
        .iter()
        .filter_map(|h| {
            let lines = h.get("lines").and_then(|l| l.as_array())?;
            Some(crate::models::session::DiffHunk {
                old_start: h.get("oldStart").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                old_lines: h.get("oldLines").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                new_start: h.get("newStart").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                new_lines: h.get("newLines").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                lines: lines
                    .iter()
                    .filter_map(|l| l.as_str().map(String::from))
                    .collect(),
            })
        })
        .collect();

    if hunks.is_empty() {
        return None;
    }
    Some(crate::models::session::DiffInfo { file_path, hunks })
}

/// Extract structured, tool-specific result metadata onto the general record model.
/// The heavy result *text* stays in `content_preview`; this only carries the small
/// structured bits a dedicated renderer needs (status, size, timing, selections).
/// Kept lean & shape-detected so we never balloon every tool_result with full payloads.
fn extract_result_meta(val: &serde_json::Value, record_type: &str) -> Option<serde_json::Value> {
    if record_type != "tool_result" {
        return None;
    }
    let tur = val.get("toolUseResult")?;

    // AskUserQuestion: { questions, answers, annotations }
    if let Some(answers) = tur.get("answers") {
        if answers.is_object() {
            return Some(serde_json::json!({ "answers": answers }));
        }
    }

    // WebFetch: { bytes, code, codeText, result, durationMs, url }
    if tur.get("code").is_some() && tur.get("result").is_some() {
        return Some(serde_json::json!({
            "webfetch": {
                "code": tur.get("code"),
                "code_text": tur.get("codeText"),
                "bytes": tur.get("bytes"),
                "duration_ms": tur.get("durationMs"),
                "url": tur.get("url"),
            }
        }));
    }

    // WebSearch: { query, results, durationSeconds, searchCount } --the readable summary
    // lives in content_preview; we only need the small stats here.
    if tur.get("results").is_some() && tur.get("query").is_some() {
        return Some(serde_json::json!({
            "websearch": {
                "count": tur.get("searchCount"),
                "duration_seconds": tur.get("durationSeconds"),
            }
        }));
    }

    // Bash / PowerShell / Monitor: { stdout, stderr, interrupted, backgroundTaskId, ... }
    if tur.get("stdout").is_some() || tur.get("stderr").is_some() {
        let stderr = tur.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
        return Some(serde_json::json!({
            "terminal": {
                "interrupted": tur.get("interrupted"),
                "background_task_id": tur.get("backgroundTaskId"),
                "exit_note": tur.get("returnCodeInterpretation"),
                // full stderr — the UI shows it in a scrollable box, never truncated
                "stderr": if stderr.is_empty() { serde_json::Value::Null }
                          else { serde_json::Value::String(stderr.to_string()) },
            }
        }));
    }

    // Read: { type: "text"|"image", file: { filePath, numLines, totalLines, ... } }
    if let Some(file) = tur.get("file") {
        return Some(serde_json::json!({
            "read": {
                "file_path": file.get("filePath"),
                "num_lines": file.get("numLines"),
                "total_lines": file.get("totalLines"),
                "start_line": file.get("startLine"),
                "is_image": tur.get("type").and_then(|t| t.as_str()) == Some("image"),
            }
        }));
    }

    // Grep: { mode, filenames, numFiles, content, numLines }
    if let Some(mode) = tur.get("mode") {
        return Some(serde_json::json!({
            "grep": {
                "mode": mode,
                "num_files": tur.get("numFiles"),
                "num_lines": tur.get("numLines"),
                "truncated": tur.get("appliedLimit"),
            }
        }));
    }

    // Glob: { filenames, numFiles, truncated, durationMs }
    if tur.get("filenames").is_some() {
        return Some(serde_json::json!({
            "glob": {
                "num_files": tur.get("numFiles"),
                "truncated": tur.get("truncated"),
            }
        }));
    }

    None
}

fn extract_tool_info(
    val: &serde_json::Value,
    record_type: &str,
) -> (Option<String>, Option<String>, Option<serde_json::Value>) {
    match record_type {
        "assistant" => {
            if let Some(msg) = val.get("message") {
                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                    for item in content {
                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let name = item.get("name").and_then(|n| n.as_str()).map(String::from);
                            let id = item.get("id").and_then(|i| i.as_str()).map(String::from);
                            let input = item.get("input").cloned();
                            return (name, id, input);
                        }
                    }
                }
            }
            (None, None, None)
        }
        "tool_result" => {
            // Try toolUseResult.tool_use_id (object form)
            let mut id = val
                .get("toolUseResult")
                .and_then(|r| r.get("tool_use_id"))
                .and_then(|i| i.as_str())
                .map(String::from);
            // Fallback: message.content[].tool_use_id
            if id.is_none() {
                if let Some(msg) = val.get("message") {
                    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                        for item in arr {
                            if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                                id = item
                                    .get("tool_use_id")
                                    .and_then(|i| i.as_str())
                                    .map(String::from);
                                if id.is_some() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            (None, id, None)
        }
        _ => (None, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempJsonl {
        path: PathBuf,
    }

    impl TempJsonl {
        fn from_bytes(bytes: &[u8]) -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "code-dejavu-jsonl-{}-{}.jsonl",
                std::process::id(),
                id
            ));
            fs::write(&path, bytes).expect("write temp jsonl");
            Self { path }
        }

        fn from_lines(lines: &[String]) -> (Self, Vec<u64>, u64) {
            let mut bytes = Vec::new();
            let mut starts = Vec::with_capacity(lines.len());
            for line in lines {
                starts.push(bytes.len() as u64);
                bytes.extend_from_slice(line.as_bytes());
                bytes.push(b'\n');
            }
            let file = Self::from_bytes(&bytes);
            (file, starts, bytes.len() as u64)
        }
    }

    impl Drop for TempJsonl {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn user_lines(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| {
                serde_json::to_string(&json!({
                    "type": "user",
                    "message": { "content": format!("消息 {index} · 中文😀") }
                }))
                .expect("serialize line")
            })
            .collect()
    }

    fn previews(page: &crate::models::session::PaginatedRecords) -> Vec<&str> {
        page.records
            .iter()
            .map(|record| record.content_preview.as_str())
            .collect()
    }

    #[test]
    fn claude_native_metadata_separates_title_and_interaction_times() {
        let lines = vec![
            json!({
                "type": "user",
                "timestamp": "2026-07-28T01:02:03Z",
                "message": {"role": "user", "content": "first prompt"}
            })
            .to_string(),
            json!({"type": "summary", "summary": "generated summary"}).to_string(),
            json!({"type": "custom-title", "customTitle": "My session"}).to_string(),
            json!({
                "type": "assistant",
                "timestamp": "2026-07-28T02:03:04Z",
                "message": {"role": "assistant", "model": "claude-test", "content": "done"}
            })
            .to_string(),
        ];
        let (file, _, _) = TempJsonl::from_lines(&lines);
        let (title, created, updated, models) = read_claude_session_native_meta(&file.path);
        assert_eq!(title.as_deref(), Some("My session"));
        assert!(created
            .as_deref()
            .is_some_and(|value| value.contains("2026-07-28")));
        assert!(updated
            .as_deref()
            .is_some_and(|value| value.contains("2026-07-28")));
        assert_eq!(models[0].model.as_deref(), Some("claude-test"));
    }

    #[test]
    fn claude_summary_includes_native_metadata() {
        let lines = vec![
            json!({"type": "summary", "summary": "must come from the index"}).to_string(),
            json!({
                "type": "assistant",
                "timestamp": "2026-07-28T02:03:04Z",
                "message": {"role": "assistant", "model": "claude-test", "content": "done"}
            })
            .to_string(),
        ];
        let (file, _, _) = TempJsonl::from_lines(&lines);
        let summary =
            read_claude_session_summary_fast(&file.path, "project", "D:\\project").unwrap();

        assert_eq!(
            summary.agent_title.as_deref(),
            Some("must come from the index")
        );
        assert_eq!(
            summary.model_contexts[0].model.as_deref(),
            Some("claude-test")
        );
        assert!(summary.updated_at.is_some());
        assert_eq!(
            summary.file_size_bytes,
            fs::metadata(&file.path).unwrap().len()
        );
    }

    #[test]
    fn skippable_drops_tool_noise_at_content() {
        assert!(skippable_at_content(
            r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#
        ));
        assert!(skippable_at_content(r#"{"toolUseResult":{"stdout":"x"}}"#));
        assert!(skippable_at_content(r#"{"type":"attachment"}"#));
        // tool_use with NO text block → skippable
        assert!(skippable_at_content(
            r#"{"message":{"content":[{"type":"tool_use"}]}}"#
        ));
        // tool_use WITH a text block must still be parsed (lead-in text survives)
        assert!(!skippable_at_content(
            r#"{"message":{"content":[{"type":"text"},{"type":"tool_use"}]}}"#
        ));
        // plain assistant text is never skippable
        assert!(!skippable_at_content(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#
        ));
    }

    #[test]
    fn streaming_assistant_text_merges_into_one_record() {
        let mut p = RecordParser::new();
        let c1 = json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"Hello "}]}});
        let c2 = json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"world"}]}});
        let fin = json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"!"}]}});
        assert!(p.push(&c1, "content").is_empty());
        assert!(p.push(&c2, "content").is_empty());
        let out = p.push(&fin, "content");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record_type, "assistant");
        assert_eq!(out[0].content_preview, "Hello world!");
    }

    #[test]
    fn tool_use_emits_lead_text_then_tool_call() {
        let mut p = RecordParser::new();
        let lead = json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"Let me check"}]}});
        let call = json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}});
        assert!(p.push(&lead, "tool").is_empty());
        let out = p.push(&call, "tool");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].record_type, "assistant");
        assert_eq!(out[0].content_preview, "Let me check");
        assert_eq!(out[1].tool_name.as_deref(), Some("Bash"));
        assert_eq!(out[1].tool_use_id.as_deref(), Some("t1"));
    }

    #[test]
    fn tool_result_extracts_text_and_id() {
        let mut p = RecordParser::new();
        let rec = json!({
            "type":"user",
            "message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]},
            "toolUseResult":{"stdout":"done"}
        });
        let out = p.push(&rec, "tool");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record_type, "tool_result");
        assert_eq!(out[0].tool_use_id.as_deref(), Some("t1"));
        assert_eq!(out[0].content_preview, "done");
    }

    #[test]
    fn thinking_block_becomes_its_own_record() {
        let mut p = RecordParser::new();
        let rec = json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"answer"}]}});
        let out = p.push(&rec, "content");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].record_type, "thinking");
        assert_eq!(out[0].content_preview, "hmm");
        assert_eq!(out[1].record_type, "assistant");
        assert_eq!(out[1].content_preview, "answer");
    }

    #[test]
    fn user_system_noise_is_debug_level() {
        let noisy = json!({"type":"user","message":{"content":"<command-name>foo</command-name>"}});
        assert_eq!(classify_level(&noisy, "user", true, false), "debug");
        let real = json!({"type":"user","message":{"content":"hello there"}});
        assert_eq!(classify_level(&real, "user", true, false), "content");
    }

    #[test]
    fn reverse_cursor_inside_multibyte_line_aligns_without_utf8_error() {
        let lines = user_lines(6);
        let (file, starts, _) = TempJsonl::from_lines(&lines);
        let chinese = lines[3]
            .as_bytes()
            .windows("中".len())
            .position(|window| window == "中".as_bytes())
            .expect("Chinese character");
        let middle_of_character = starts[3] + chinese as u64 + 1;

        let page = read_claude_records_before(&file.path, middle_of_character, 100, "content")
            .expect("reverse page");

        assert_eq!(
            previews(&page),
            vec!["消息 0 · 中文😀", "消息 1 · 中文😀", "消息 2 · 中文😀"]
        );
        assert_eq!(page.start_byte_offset, 0);
        assert_eq!(page.next_byte_offset, starts[3]);
        assert!(!page.has_earlier);
        assert!(page.has_more);
    }

    #[test]
    fn tail_returns_exact_suffix_in_chronological_order() {
        let lines = user_lines(8);
        let (file, starts, file_size) = TempJsonl::from_lines(&lines);

        let page = read_claude_records_tail(&file.path, 3, "content").expect("tail page");

        assert_eq!(
            previews(&page),
            vec!["消息 5 · 中文😀", "消息 6 · 中文😀", "消息 7 · 中文😀"]
        );
        assert_eq!(page.start_byte_offset, starts[5]);
        assert_eq!(page.next_byte_offset, file_size);
        assert!(page.has_earlier);
        assert!(!page.has_more);
        assert_eq!(
            page.records
                .iter()
                .map(|record| record.byte_offset)
                .collect::<Vec<_>>(),
            starts[5..].to_vec()
        );
    }

    #[test]
    fn tail_grows_past_a_large_multibyte_source_line() {
        let mut lines = user_lines(3);
        lines[0] = serde_json::to_string(&json!({
            "type": "user",
            "message": { "content": "中😀".repeat(20_000) }
        }))
        .expect("serialize large line");
        let (file, starts, _) = TempJsonl::from_lines(&lines);

        let page = read_claude_records_tail(&file.path, 2, "content").expect("growing tail");

        assert_eq!(previews(&page), vec!["消息 1 · 中文😀", "消息 2 · 中文😀"]);
        assert_eq!(page.start_byte_offset, starts[1]);
        assert!(page.has_earlier);
    }

    #[test]
    fn consecutive_reverse_pages_are_ordered_and_do_not_overlap() {
        let lines = user_lines(11);
        let (file, _, _) = TempJsonl::from_lines(&lines);

        let newest = read_claude_records_tail(&file.path, 4, "content").expect("tail");
        let middle = read_claude_records_before(&file.path, newest.start_byte_offset, 4, "content")
            .expect("middle");
        let oldest = read_claude_records_before(&file.path, middle.start_byte_offset, 4, "content")
            .expect("oldest");

        assert_eq!(
            previews(&oldest),
            vec!["消息 0 · 中文😀", "消息 1 · 中文😀", "消息 2 · 中文😀"]
        );
        assert_eq!(
            previews(&middle),
            vec![
                "消息 3 · 中文😀",
                "消息 4 · 中文😀",
                "消息 5 · 中文😀",
                "消息 6 · 中文😀"
            ]
        );
        assert_eq!(
            previews(&newest),
            vec![
                "消息 7 · 中文😀",
                "消息 8 · 中文😀",
                "消息 9 · 中文😀",
                "消息 10 · 中文😀"
            ]
        );
        let all_offsets: Vec<u64> = oldest
            .records
            .iter()
            .chain(&middle.records)
            .chain(&newest.records)
            .map(|record| record.byte_offset)
            .collect();
        assert!(all_offsets.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!oldest.has_earlier);
        assert!(middle.has_earlier);
        assert!(newest.has_earlier);
    }

    #[test]
    fn forward_page_keeps_all_records_from_one_source_line() {
        let lines = vec![
            serde_json::to_string(&json!({
                "type":"assistant",
                "message": {
                    "stop_reason":"end_turn",
                    "content":[
                        {"type":"thinking","thinking":"reason"},
                        {"type":"text","text":"answer"}
                    ]
                }
            }))
            .expect("serialize assistant"),
            user_lines(1).into_iter().next().expect("user line"),
        ];
        let (file, starts, _) = TempJsonl::from_lines(&lines);

        let page = read_claude_records_seekable(&file.path, 0, 1, "content").expect("forward page");

        assert_eq!(previews(&page), vec!["reason", "answer"]);
        assert_eq!(page.next_byte_offset, starts[1]);
        assert!(page.has_more);
    }

    #[test]
    fn forward_reader_skips_invalid_utf8_line_and_advances_cursor() {
        let valid = user_lines(1).into_iter().next().expect("user line");
        let mut bytes = vec![0xff, 0xfe, b'\n'];
        bytes.extend_from_slice(valid.as_bytes());
        bytes.push(b'\n');
        let file = TempJsonl::from_bytes(&bytes);

        let page = read_claude_records_seekable(&file.path, 0, 1, "content")
            .expect("forward page after invalid UTF-8");

        assert_eq!(previews(&page), vec!["消息 0 · 中文😀"]);
        assert!(page.next_byte_offset > 0);
        assert_eq!(page.next_byte_offset, bytes.len() as u64);
        assert!(!page.has_more);
    }

    #[test]
    fn only_tail_flushes_pending_stream_and_anchors_it_to_last_line() {
        let lines = vec![
            serde_json::to_string(&json!({
                "type":"assistant",
                "message":{"stop_reason":null,"content":[{"type":"text","text":"first "}]}
            }))
            .expect("serialize first chunk"),
            serde_json::to_string(&json!({
                "type":"assistant",
                "message":{"stop_reason":null,"content":[{"type":"text","text":"second"}]}
            }))
            .expect("serialize second chunk"),
        ];
        let (file, starts, file_size) = TempJsonl::from_lines(&lines);

        let tail = read_claude_records_tail(&file.path, 10, "content").expect("tail");
        let explicit_before =
            read_claude_records_before(&file.path, file_size, 10, "content").expect("before EOF");

        assert_eq!(previews(&tail), vec!["first second"]);
        assert_eq!(tail.start_byte_offset, starts[1]);
        assert_eq!(tail.records[0].byte_offset, starts[1]);
        assert!(explicit_before.records.is_empty());
    }
}
