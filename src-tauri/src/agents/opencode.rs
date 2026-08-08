//! OpenCode provider — SQLite store at `~/.local/share/opencode/opencode.db` (Drizzle schema).
//!
//! Unlike Claude/Codex (JSONL files + the generic LineParser/pager), OpenCode keeps everything in
//! SQLite: `session → message → part`. So this provider is DB-driven — it queries messages+parts
//! and builds `SessionRecord`s directly, repurposing `byte_offset` as a message-row cursor for
//! pagination. A `tool` part already bundles call+result (state.input/output); we split it into a
//! call record + a result record sharing the callID so the existing frontend pairing works.

use super::{
    quote_command_arg, AgentProvider, Capabilities, FastIndexTextCollector, IndexBatch, IndexDoc,
    InstructionCandidate, TokenUsage,
};
use crate::error::AppError;
use crate::hosts::Host;
use crate::models::profile::ProfileArchive;
use crate::models::rule::RuleFile;
use crate::models::session::{
    push_model_context, PaginatedRecords, SessionModelInfo, SessionRecord, SessionSearchHit,
    SessionSummary, SubagentInfo,
};
use crate::paths::app_data_dir;
use crate::services::profile_archiver::{self, SnapshotItem, SnapshotSpec};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            let l: chrono::DateTime<chrono::Local> = dt.into();
            l.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_default()
}

fn top_session(parent: &HashMap<String, Option<String>>, mut id: String) -> String {
    for _ in 0..16 {
        match parent.get(&id) {
            Some(Some(p)) => id = p.clone(),
            _ => break,
        }
    }
    id
}

fn value_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn normalized_variant(v: &Value) -> Option<String> {
    value_str(v, "variant").filter(|variant| variant != "default")
}

fn push_opencode_model_value(contexts: &mut Vec<SessionModelInfo>, value: &Value) {
    match value {
        Value::String(model) => {
            push_model_context(contexts, None, Some(model.clone()), None);
        }
        Value::Object(_) => {
            push_model_context(
                contexts,
                value_str(value, "providerID"),
                value_str(value, "modelID").or_else(|| value_str(value, "id")),
                normalized_variant(value),
            );
        }
        _ => {}
    }
}

fn push_opencode_context_from_data(contexts: &mut Vec<SessionModelInfo>, data: &Value) {
    if data.get("providerID").is_some() || data.get("modelID").is_some() {
        push_model_context(
            contexts,
            value_str(data, "providerID"),
            value_str(data, "modelID"),
            normalized_variant(data),
        );
    }
    if let Some(model) = data.get("model") {
        push_opencode_model_value(contexts, model);
    }
}

pub struct OpenCodeProvider {
    db: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    archive_root: PathBuf,
    /// The machine this install lives on. Session rows store the project directory as the agent
    /// saw it, so a WSL install's directories need translating before they can be opened here.
    host: Host,
}

struct OpenCodeMessageBatch {
    cursor: u64,
    records: Vec<SessionRecord>,
}

impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeProvider {
    pub fn new() -> Self {
        Self::for_host(Host::Native, &home())
    }

    /// An OpenCode install rooted at `home`, which may belong to another host (e.g. a WSL distro).
    /// Snapshots stay in the app's own data directory, namespaced per host so two installs cannot
    /// overwrite each other's archives.
    pub fn for_host(host: Host, home: &Path) -> Self {
        let data_dir = home.join(".local").join("share").join("opencode");
        let archive_root = match host.tag() {
            Some(_) => app_data_dir()
                .join("archives")
                .join("opencode")
                .join(format!("wsl-{}", host.key())),
            None => app_data_dir().join("archives").join("opencode"),
        };
        Self {
            db: data_dir.join("opencode.db"),
            config_dir: home.join(".config").join("opencode"),
            archive_root,
            host,
            data_dir,
        }
    }

    fn config_file_jsonc(&self) -> PathBuf {
        self.config_dir.join("opencode.jsonc")
    }

    fn config_file_json(&self) -> PathBuf {
        self.config_dir.join("opencode.json")
    }

    fn snapshot_spec(&self) -> SnapshotSpec {
        SnapshotSpec {
            source: "opencode",
            display_name: "OpenCode",
            archive_root: self.archive_root.clone(),
            items: vec![
                SnapshotItem {
                    name: "config",
                    path: self.config_dir.clone(),
                    preserve: &[],
                },
                SnapshotItem {
                    name: "data",
                    path: self.data_dir.clone(),
                    preserve: &["auth.json", "account.json"],
                },
            ],
            clear_current_on_create: true,
        }
    }

    fn push_roots_from_query(
        conn: &Connection,
        host: &Host,
        roots: &mut Vec<PathBuf>,
        seen: &mut std::collections::HashSet<String>,
        sql: &str,
    ) {
        let Ok(mut stmt) = conn.prepare(sql) else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
            return;
        };
        for row in rows.flatten() {
            let root = row.trim();
            if root.is_empty() || root == "global" {
                continue;
            }
            let path = host.to_readable(root);
            if !path.is_absolute() || !seen.insert(path.to_string_lossy().to_string()) {
                continue;
            }
            roots.push(path);
        }
    }

    fn project_roots_from_db(&self) -> Vec<PathBuf> {
        let Ok(conn) = self.conn() else {
            return Vec::new();
        };
        let mut roots = Vec::new();
        let mut seen = std::collections::HashSet::new();
        Self::push_roots_from_query(
            &conn,
            &self.host,
            &mut roots,
            &mut seen,
            "SELECT directory FROM session WHERE directory IS NOT NULL AND directory <> ''",
        );
        Self::push_roots_from_query(
            &conn,
            &self.host,
            &mut roots,
            &mut seen,
            "SELECT id FROM project WHERE id IS NOT NULL AND id <> ''",
        );
        Self::push_roots_from_query(
            &conn,
            &self.host,
            &mut roots,
            &mut seen,
            "SELECT directory FROM project_directory WHERE directory IS NOT NULL AND directory <> ''",
        );
        Self::push_roots_from_query(
            &conn,
            &self.host,
            &mut roots,
            &mut seen,
            "SELECT path FROM project_directory WHERE path IS NOT NULL AND path <> ''",
        );
        roots.sort();
        roots
    }

    fn read_project_agents_rules(&self) -> Result<Vec<RuleFile>, AppError> {
        let mut rules = Vec::new();
        for project_path in self.project_roots_from_db() {
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

    fn archive_db(&self, archive_name: &str) -> PathBuf {
        self.archive_root
            .join(archive_name)
            .join("data")
            .join("opencode.db")
    }

    fn db_sources(&self) -> Vec<(PathBuf, Option<String>)> {
        let mut sources = Vec::new();
        if self.db.exists() {
            sources.push((self.db.clone(), None));
        }
        if let Ok(archives) = fs::read_dir(&self.archive_root) {
            for entry in archives.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let archive_name = entry.file_name().to_string_lossy().to_string();
                let db = entry.path().join("data").join("opencode.db");
                if db.exists() {
                    sources.push((db, Some(archive_name)));
                }
            }
        }
        sources
    }

    fn has_any_db(&self) -> bool {
        !self.db_sources().is_empty()
    }

    fn open_db(path: &Path) -> Result<Connection, AppError> {
        if !path.exists() {
            return Err(AppError::NotFound(format!(
                "OpenCode database not found: {}",
                path.to_string_lossy()
            )));
        }
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| AppError::Archive(format!("open opencode.db: {}", e)))
    }

    fn conn(&self) -> Result<Connection, AppError> {
        Self::open_db(&self.db)
    }

    fn conn_for_archive(&self, archive: Option<&str>) -> Result<Connection, AppError> {
        let path = match archive.filter(|name| !name.trim().is_empty()) {
            Some(name) => self.archive_db(name),
            None => self.db.clone(),
        };
        Self::open_db(&path)
    }

    fn list_sessions_from_conn(
        conn: &Connection,
        host: &Host,
        project: Option<&str>,
        archive_name: Option<String>,
    ) -> Result<Vec<SessionSummary>, AppError> {
        let map_count = |sql: &str| -> HashMap<String, i64> {
            let mut m = HashMap::new();
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                {
                    for row in rows.flatten() {
                        m.insert(row.0, row.1);
                    }
                }
            }
            m
        };
        let msg_counts = map_count("SELECT session_id, count(*) FROM message GROUP BY session_id");
        let child_counts =
            map_count("SELECT parent_id, count(*) FROM session WHERE parent_id IS NOT NULL GROUP BY parent_id");
        let model_contexts = Self::model_contexts_by_top(conn);

        let has_updated = conn
            .prepare("SELECT time_updated FROM session LIMIT 0")
            .is_ok();
        let sql = if has_updated {
            "SELECT id, directory, title, time_created, time_updated \
             FROM session WHERE parent_id IS NULL ORDER BY time_updated DESC"
        } else {
            "SELECT id, directory, title, time_created, time_created \
             FROM session WHERE parent_id IS NULL ORDER BY time_created DESC"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            })
            .map_err(|e| AppError::Archive(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows.flatten() {
            let (id, dir, title, tcreated, tupdated) = row;
            let dir = dir.unwrap_or_default();
            if let Some(p) = project {
                if dir != p {
                    continue;
                }
            }
            let rc = *msg_counts.get(&id).unwrap_or(&0) as u32;
            out.push(SessionSummary {
                source: "opencode".to_string(),
                session_id: id.clone(),
                project: dir.clone(),
                project_path: host.to_readable(&dir).to_string_lossy().to_string(),
                first_prompt: None,
                agent_title: title
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty()),
                created_at: tcreated.map(fmt_ms),
                updated_at: tupdated.map(fmt_ms),
                timestamp: tupdated.or(tcreated).map(fmt_ms),
                file_size_bytes: (rc as u64) * 400,
                subagent_count: *child_counts.get(&id).unwrap_or(&0) as u32,
                archive_name: archive_name.clone(),
                model_contexts: model_contexts.get(&id).cloned().unwrap_or_default(),
            });
        }
        Ok(out)
    }

    fn index_documents_from_conn(
        &self,
        conn: &Connection,
        archive_name: Option<String>,
    ) -> Vec<IndexDoc> {
        // Resolve any (possibly child) session up to its top-level session.
        let mut parent: HashMap<String, Option<String>> = HashMap::new();
        if let Ok(mut s) = conn.prepare("SELECT id, parent_id FROM session") {
            if let Ok(rows) = s.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            }) {
                for row in rows.flatten() {
                    parent.insert(row.0, row.1);
                }
            }
        }
        let top_of = |mut id: String| -> String {
            for _ in 0..16 {
                match parent.get(&id) {
                    Some(Some(p)) => id = p.clone(),
                    _ => break,
                }
            }
            id
        };
        // Gather kind-tagged text per top-level session (child sessions roll up to their parent).
        let mut by_top: HashMap<String, FastIndexTextCollector> = HashMap::new();
        let mut first_by_top: HashMap<String, String> = HashMap::new();
        let mut tokens_by_top: HashMap<String, TokenUsage> = HashMap::new();
        if let Ok(mut s) = conn.prepare("SELECT session_id, data FROM part") {
            if let Ok(rows) =
                s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for row in rows.flatten() {
                    let (sid, data) = row;
                    let Ok(v) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    // `step-finish` parts carry per-step token usage; sum them per top session.
                    if v.get("type").and_then(|t| t.as_str()) == Some("step-finish") {
                        let tk = v.get("tokens");
                        let g = |k: &str| {
                            tk.and_then(|t| t.get(k))
                                .and_then(|x| x.as_u64())
                                .unwrap_or(0)
                        };
                        let cache = tk
                            .and_then(|t| t.get("cache"))
                            .map(|c| {
                                c.get("read").and_then(|x| x.as_u64()).unwrap_or(0)
                                    + c.get("write").and_then(|x| x.as_u64()).unwrap_or(0)
                            })
                            .unwrap_or(0);
                        let input = g("input");
                        let output = g("output") + g("reasoning");
                        let e = tokens_by_top.entry(top_of(sid)).or_default();
                        e.input_tokens += input;
                        e.output_tokens += output;
                        e.cache_tokens += cache;
                        e.total_tokens += input + output + cache;
                        continue;
                    }
                    let (kind, text) = match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "text" => (
                            "content",
                            v.get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ),
                        "reasoning" => (
                            "reasoning",
                            v.get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ),
                        "tool" => {
                            let st = v.get("state");
                            let inp = st
                                .and_then(|s| s.get("input"))
                                .map(|i| i.to_string())
                                .unwrap_or_default();
                            let out = match st.and_then(|s| s.get("output")) {
                                Some(Value::String(s)) => s.clone(),
                                Some(o) => o.to_string(),
                                None => String::new(),
                            };
                            ("tool", format!("{} {}", inp, out))
                        }
                        _ => continue,
                    };
                    if text.trim().is_empty() {
                        continue;
                    }
                    let top = top_of(sid);
                    if kind == "content" {
                        first_by_top
                            .entry(top.clone())
                            .or_insert_with(|| text.chars().take(200).collect());
                    }
                    by_top.entry(top).or_default().push(kind, text);
                }
            }
        }
        let mut docs = Vec::new();
        for s in
            Self::list_sessions_from_conn(conn, &self.host, None, archive_name).unwrap_or_default()
        {
            // Keep content-less sessions in the index too (matches Claude + the old disk-scan
            // browse), so an empty session still appears and can be resumed.
            let texts = by_top
                .remove(&s.session_id)
                .unwrap_or_default()
                .into_texts();
            // OpenCode is DB-backed (no per-file mtime), so it uses the engine's full-reparse
            // fallback; key/version are still set for the persisted cache + stable identity.
            let key = format!(
                "opencode::{}::{}",
                s.archive_name.as_deref().unwrap_or(""),
                s.session_id
            );
            let version = format!(
                "{}:{}",
                s.file_size_bytes,
                s.timestamp.as_deref().unwrap_or("")
            );
            docs.push(IndexDoc {
                source: "opencode".to_string(),
                session_id: s.session_id.clone(),
                project: s.project,
                project_path: s.project_path,
                created_at: s.created_at,
                updated_at: s.updated_at,
                agent_title: s.agent_title,
                timestamp: s.timestamp,
                file_size_bytes: s.file_size_bytes,
                subagent_count: s.subagent_count,
                archive_name: s.archive_name,
                first_prompt: first_by_top.remove(&s.session_id),
                model_contexts: s.model_contexts,
                texts,
                tokens: tokens_by_top.remove(&s.session_id).unwrap_or_default(),
                key,
                version,
            });
        }
        docs
    }

    fn model_contexts_by_top(conn: &Connection) -> HashMap<String, Vec<SessionModelInfo>> {
        let mut parent: HashMap<String, Option<String>> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT id, parent_id FROM session") {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            }) {
                for row in rows.flatten() {
                    parent.insert(row.0, row.1);
                }
            }
        }

        let mut contexts: HashMap<String, Vec<SessionModelInfo>> = HashMap::new();

        if let Ok(mut stmt) =
            conn.prepare("SELECT id, model FROM session WHERE model IS NOT NULL AND model <> ''")
        {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for row in rows.flatten() {
                    let (sid, data) = row;
                    let Ok(value) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    let top = top_session(&parent, sid);
                    push_opencode_model_value(contexts.entry(top).or_default(), &value);
                }
            }
        }

        if let Ok(mut stmt) = conn.prepare(
            r#"SELECT session_id, data FROM message
               WHERE data LIKE '%"model%' OR data LIKE '%"providerID"%' OR data LIKE '%"modelID"%'"#,
        ) {
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for row in rows.flatten() {
                    let (sid, data) = row;
                    let Ok(value) = serde_json::from_str::<Value>(&data) else { continue };
                    let top = top_session(&parent, sid);
                    push_opencode_context_from_data(contexts.entry(top).or_default(), &value);
                }
            }
        }

        contexts
    }

    fn message_count(conn: &Connection, session_id: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM message WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }

    /// Parse a half-open message-cursor range. A message is cursor-atomic because one OpenCode
    /// part can emit a call/result pair that must not be split across pages.
    fn message_batches(
        conn: &Connection,
        session_id: &str,
        start: i64,
        end: i64,
        min_level: &str,
    ) -> Result<Vec<OpenCodeMessageBatch>, AppError> {
        if start >= end {
            return Ok(Vec::new());
        }
        let mut message_stmt = conn
            .prepare(
                "SELECT id, data, time_created FROM message \
                 WHERE session_id=?1 ORDER BY time_created, id LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let rows = message_stmt
            .query_map(params![session_id, end - start, start], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let messages: Vec<(String, String, i64)> = rows
            .collect::<Result<_, _>>()
            .map_err(|e| AppError::Archive(e.to_string()))?;

        let mut part_stmt = conn
            .prepare("SELECT data FROM part WHERE message_id=?1 ORDER BY time_created, id")
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let mut batches = Vec::new();
        for (position, (message_id, message_data, created)) in messages.into_iter().enumerate() {
            let cursor = start as u64 + position as u64;
            let role = serde_json::from_str::<Value>(&message_data)
                .ok()
                .and_then(|value| {
                    value
                        .get("role")
                        .and_then(|role| role.as_str())
                        .map(String::from)
                })
                .unwrap_or_default();
            let timestamp = Some(fmt_ms_full(created));
            let rows = part_stmt
                .query_map([&message_id], |row| row.get::<_, String>(0))
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let parts: Vec<String> = rows
                .collect::<Result<_, _>>()
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let mut records = Vec::new();
            for data in parts {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    build_part(&role, &value, &timestamp, min_level, &mut records);
                }
            }
            for (index, record) in records.iter_mut().enumerate() {
                record.byte_offset = cursor * 1000 + index as u64;
            }
            if !records.is_empty() {
                batches.push(OpenCodeMessageBatch { cursor, records });
            }
        }
        Ok(batches)
    }
}

impl AgentProvider for OpenCodeProvider {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn display_name(&self) -> &'static str {
        "OpenCode"
    }
    fn available(&self) -> bool {
        self.config_dir.exists() || self.data_dir.exists() || self.has_any_db()
    }
    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.data_dir.clone(), self.archive_root.clone()]
    }
    fn capabilities(&self) -> Capabilities {
        let has_live_db = self.db.exists();
        let has_any_db = self.has_any_db();
        Capabilities {
            sessions_read: has_any_db,
            sessions_search: has_any_db,
            sessions_resume: has_live_db,
            sessions_subagents: has_any_db, // child sessions (parent_id), linked via task.metadata.sessionId
            rules_read: has_live_db,
            rules_write: false,
            memory_read: false,
            memory_write: false,
            instructions_read: true,
            instructions_write: true,
            archive_read: true,
            archive_write: true,
            config_format: "jsonc",
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
        let conn = match self.conn() {
            Ok(conn) => conn,
            Err(AppError::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Self::list_sessions_from_conn(&conn, &self.host, project, None)
    }

    fn index_documents(&self) -> IndexBatch {
        let mut docs = Vec::new();
        let mut failed = 0;
        for (db, archive_name) in self.db_sources() {
            match Self::open_db(&db) {
                Ok(conn) => docs.extend(self.index_documents_from_conn(&conn, archive_name)),
                Err(_) => failed += 1,
            }
        }
        IndexBatch { docs, failed }
    }

    fn global_instruction_candidates(&self) -> Vec<InstructionCandidate> {
        vec![
            InstructionCandidate {
                title: "全局 opencode.jsonc".to_string(),
                scope: "global",
                kind: "config",
                path: self.config_file_jsonc(),
                editable: true,
                include_missing: true,
                exists: None,
                size_bytes: None,
                description: "OpenCode 全局 JSONC 配置文件。".to_string(),
            },
            InstructionCandidate {
                title: "全局 opencode.json".to_string(),
                scope: "global",
                kind: "config",
                path: self.config_file_json(),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "OpenCode 全局 JSON 配置文件。".to_string(),
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
                description: "OpenCode 项目指令文件。".to_string(),
            },
            InstructionCandidate {
                title: "项目 opencode.json".to_string(),
                scope: "project",
                kind: "config",
                path: project_path.join("opencode.json"),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "OpenCode 项目 JSON 配置文件。".to_string(),
            },
            InstructionCandidate {
                title: "项目 opencode.jsonc".to_string(),
                scope: "project",
                kind: "config",
                path: project_path.join("opencode.jsonc"),
                editable: true,
                include_missing: false,
                exists: None,
                size_bytes: None,
                description: "OpenCode 项目 JSONC 配置文件。".to_string(),
            },
        ]
    }

    fn instruction_project_roots(&self) -> Vec<PathBuf> {
        self.project_roots_from_db()
    }

    fn list_rules(&self) -> Result<Vec<RuleFile>, AppError> {
        self.read_project_agents_rules()
    }

    fn resume_command(&self, session_id: &str, extra_args: &[String]) -> Option<String> {
        Some(
            format!(
                "opencode --session {} {}",
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
        let conn = self.conn_for_archive(archive).ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT m.data, p.data FROM message m \
                 JOIN part p ON p.message_id=m.id \
                 WHERE m.session_id=?1 ORDER BY m.time_created, p.time_created LIMIT 100",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ok()?;
        for row in rows.flatten() {
            let Ok(message) = serde_json::from_str::<Value>(&row.0) else {
                continue;
            };
            if message.get("role").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }
            let Ok(part) = serde_json::from_str::<Value>(&row.1) else {
                continue;
            };
            if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    let text = text.trim();
                    if !text.is_empty() {
                        return Some(text.chars().take(200).collect());
                    }
                }
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
        let conn = self.conn_for_archive(archive)?;
        let total = Self::message_count(&conn, session_id);
        let start = (byte_offset as i64).min(total).max(0);
        let mut cursor = start;
        let mut records = Vec::new();
        if limit == 0 {
            return Ok(PaginatedRecords {
                records,
                start_byte_offset: start as u64,
                next_byte_offset: start as u64,
                has_earlier: start > 0,
                has_more: start < total,
            });
        }

        'outer: while cursor < total {
            let batch_end = (cursor + 200).min(total);
            let batches = Self::message_batches(&conn, session_id, cursor, batch_end, min_level)?;
            for batch in batches {
                cursor = batch.cursor as i64 + 1;
                records.extend(batch.records);
                if records.len() >= limit as usize {
                    break 'outer;
                }
            }
            // Empty/filtered messages are still consumed by the database cursor.
            if cursor < batch_end {
                cursor = batch_end;
            }
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
        let conn = self.conn_for_archive(archive)?;
        let total = Self::message_count(&conn, session_id) as u64;
        drop(conn);
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
        let conn = self.conn_for_archive(archive)?;
        let total = Self::message_count(&conn, session_id).max(0) as u64;
        let boundary = before_offset.min(total);
        if limit == 0 || boundary == 0 {
            return Ok(PaginatedRecords {
                records: Vec::new(),
                start_byte_offset: boundary,
                next_byte_offset: boundary,
                has_earlier: false,
                has_more: boundary < total,
            });
        }

        let target = limit as usize;
        let mut window = ((limit as u64).saturating_mul(3)).max(64).min(boundary);
        loop {
            let start = boundary.saturating_sub(window);
            let batches =
                Self::message_batches(&conn, session_id, start as i64, boundary as i64, min_level)?;
            let mut selected_start = batches.len();
            let mut selected_count = 0_usize;
            while selected_start > 0 && selected_count < target {
                selected_start -= 1;
                selected_count += batches[selected_start].records.len();
            }
            let at_start = start == 0;
            if at_start || (selected_count >= target && selected_start > 0) {
                let has_earlier = selected_start > 0;
                let start_byte_offset = batches
                    .get(selected_start)
                    .map(|batch| batch.cursor)
                    .unwrap_or(0);
                let records = batches
                    .into_iter()
                    .skip(selected_start)
                    .flat_map(|batch| batch.records)
                    .collect();
                return Ok(PaginatedRecords {
                    records,
                    start_byte_offset,
                    next_byte_offset: boundary,
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
        let conn = self.conn_for_archive(archive)?;
        // Child sessions of this session (each is a subagent run).
        let mut kids: Vec<(String, String)> = Vec::new();
        {
            let mut s = conn
                .prepare("SELECT id, title FROM session WHERE parent_id=?1 ORDER BY time_created")
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = s
                .query_map([session_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                })
                .map_err(|e| AppError::Archive(e.to_string()))?;
            for row in rows.flatten() {
                kids.push(row);
            }
        }
        if kids.is_empty() {
            return Ok(Vec::new());
        }
        // Map child_session_id → (task callID, agent type) from the parent's `task` tool parts.
        let mut link: HashMap<String, (String, String)> = HashMap::new();
        {
            let mut s = conn
                .prepare(
                    "SELECT data FROM part WHERE session_id=?1 AND data LIKE '%\"tool\":\"task\"%'",
                )
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = s
                .query_map([session_id], |r| r.get::<_, String>(0))
                .map_err(|e| AppError::Archive(e.to_string()))?;
            for data in rows.flatten() {
                let Ok(v) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                let call_id = v
                    .get("callID")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let st = v.get("state");
                let child = st
                    .and_then(|s| s.get("metadata"))
                    .and_then(|m| m.get("sessionId"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let agent = st
                    .and_then(|s| s.get("metadata"))
                    .and_then(|m| m.get("agent"))
                    .and_then(|x| x.as_str())
                    .or_else(|| {
                        st.and_then(|s| s.get("input"))
                            .and_then(|i| i.get("subagent_type"))
                            .and_then(|x| x.as_str())
                    })
                    .unwrap_or("subagent")
                    .to_string();
                if !child.is_empty() {
                    link.insert(child.to_string(), (call_id, agent));
                }
            }
        }
        let mut mc: HashMap<String, i64> = HashMap::new();
        {
            let mut s = conn
                .prepare("SELECT session_id, count(*) FROM message GROUP BY session_id")
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = s
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| AppError::Archive(e.to_string()))?;
            for row in rows.flatten() {
                mc.insert(row.0, row.1);
            }
        }
        let mut out = Vec::new();
        for (id, title) in kids {
            let (tool_use_id, agent_type) = link.get(&id).cloned().unwrap_or_default();
            out.push(SubagentInfo {
                agent_id: id.clone(),
                agent_type: if agent_type.is_empty() {
                    "subagent".to_string()
                } else {
                    agent_type
                },
                description: title,
                tool_use_id,
                record_count: *mc.get(&id).unwrap_or(&0) as u32,
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
        // A child session IS a session — render it at the "tool" level like Claude subagents.
        self.session_detail(project, agent_id, byte_offset, limit, "tool", archive)
    }

    fn search_in_session(
        &self,
        _project: &str,
        session_id: &str,
        query: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let conn = self.conn_for_archive(archive)?;
        // Map each message id → its row index (= byte_offset cursor) so a hit can jump there.
        let mut order: Vec<String> = Vec::new();
        {
            let mut s = conn
                .prepare("SELECT id FROM message WHERE session_id=?1 ORDER BY time_created, id")
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = s
                .query_map([session_id], |r| r.get::<_, String>(0))
                .map_err(|e| AppError::Archive(e.to_string()))?;
            for row in rows.flatten() {
                order.push(row);
            }
        }
        let idx_of: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();

        let q = query.to_lowercase();
        let mut hits = Vec::new();
        let mut s = conn
            .prepare(
                "SELECT message_id, data FROM part WHERE session_id=?1 ORDER BY time_created, id",
            )
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let rows = s
            .query_map([session_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| AppError::Archive(e.to_string()))?;
        for row in rows.flatten() {
            let (mid, data) = row;
            let Ok(v) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            // Cover the same scopes as global search: conversation, reasoning, and tool I/O.
            let (record_type, text): (&str, String) = match v.get("type").and_then(|t| t.as_str()) {
                Some("text") => (
                    "message",
                    v.get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                Some("reasoning") => (
                    "thinking",
                    v.get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                Some("tool") => {
                    let st = v.get("state");
                    let inp = st
                        .and_then(|s| s.get("input"))
                        .map(|i| i.to_string())
                        .unwrap_or_default();
                    let out = match st.and_then(|s| s.get("output")) {
                        Some(Value::String(s)) => s.clone(),
                        Some(o) => o.to_string(),
                        None => String::new(),
                    };
                    ("tool_result", format!("{} {}", inp, out))
                }
                _ => continue,
            };
            if text.is_empty() || !text.to_lowercase().contains(&q) {
                continue;
            }
            hits.push(SessionSearchHit {
                byte_offset: *idx_of.get(mid.as_str()).unwrap_or(&0) as u64,
                snippet: crate::services::search::extract_snippet(&text, &q),
                record_type: record_type.to_string(),
                timestamp: None,
            });
        }
        Ok(hits)
    }
}

/// Millisecond epoch → full local timestamp (seconds precision) for records.
fn fmt_ms_full(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            let l: chrono::DateTime<chrono::Local> = dt.into();
            l.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_default()
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

/// Strip OpenCode's `read` output wrapper (`<path>…</path><type>…</type><content>BODY</content>`)
/// down to BODY. Returns the input unchanged if the wrapper isn't present.
fn clean_read_output(s: &str) -> String {
    if let (Some(i), Some(j)) = (s.find("<content>"), s.rfind("</content>")) {
        if j >= i + "<content>".len() {
            return s[i + "<content>".len()..j].trim_matches('\n').to_string();
        }
    }
    s.to_string()
}

/// Build the AskUserQuestion result_meta from a `question` tool state: a map of
/// `question_text → [selected labels]`, paired positionally from input.questions + metadata.answers.
fn build_question_answers(state: &Value) -> Option<Value> {
    let questions = state.get("input")?.get("questions")?.as_array()?;
    let answers = state
        .get("metadata")
        .and_then(|m| m.get("answers"))
        .and_then(|a| a.as_array());
    let mut map = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        if let Some(qtext) = q.get("question").and_then(|x| x.as_str()) {
            let ans = answers
                .and_then(|a| a.get(i))
                .cloned()
                .unwrap_or_else(|| Value::Array(vec![]));
            map.insert(qtext.to_string(), ans);
        }
    }
    if map.is_empty() {
        None
    } else {
        Some(json!({ "answers": map }))
    }
}

/// Map one OpenCode `part` (within a message of `role`) to 0+ display records.
fn build_part(
    role: &str,
    p: &Value,
    ts: &Option<String>,
    min_level: &str,
    out: &mut Vec<SessionRecord>,
) {
    let mut emit = |r: SessionRecord| {
        if keep(&r.level, min_level) {
            out.push(r);
        }
    };
    match p.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "text" => {
            let text = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if !text.trim().is_empty() {
                let rt = if role == "assistant" {
                    "assistant"
                } else {
                    "user"
                };
                emit(rec(rt, text.to_string(), "content", ts));
            }
        }
        "reasoning" => {
            let text = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if !text.trim().is_empty() {
                emit(rec("thinking", text.to_string(), "content", ts));
            }
        }
        "tool" => {
            // One part = call + result. Split into a call record and a result record (same callID).
            let name = p.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
            let call_id = p.get("callID").and_then(|t| t.as_str());
            let state = p.get("state");
            let status = state
                .and_then(|s| s.get("status"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            // The interactive question tool belongs in the conversation (like Claude's AskUserQuestion).
            let level = if name == "question" {
                "content"
            } else {
                "tool"
            };
            // `task` spawns a child session → render via the frontend's Agent (subagent) branch.
            let display_name = if name == "task" { "Agent" } else { name };
            let mut call = rec("assistant", String::new(), level, ts);
            call.tool_name = Some(display_name.to_string());
            call.tool_use_id = call_id.map(String::from);
            call.tool_input = if name == "edit" {
                // Render the unified diff (metadata.diff) via the ApplyPatch colorizer.
                state
                    .and_then(|s| s.get("metadata"))
                    .and_then(|m| m.get("diff"))
                    .and_then(|d| d.as_str())
                    .map(|diff| json!({ "input": diff }))
                    .or_else(|| state.and_then(|s| s.get("input")).cloned())
            } else {
                // Normalize camelCase `filePath` → `file_path` so the file cards (FileRead) bind.
                let mut input = state.and_then(|s| s.get("input")).cloned();
                if let Some(Value::Object(ref mut m)) = input {
                    if let Some(fp) = m.remove("filePath") {
                        m.insert("file_path".to_string(), fp);
                    }
                }
                input
            };
            emit(call);

            if status != "running" {
                let mut output = match state.and_then(|s| s.get("output")) {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                // OpenCode `read` wraps the body in <path>/<type>/<content>…</content>; unwrap it.
                if name == "read" {
                    output = clean_read_output(&output);
                } else if name == "write" {
                    // Show the file we wrote (input.content) instead of "Wrote file successfully.".
                    if let Some(c) = state
                        .and_then(|s| s.get("input"))
                        .and_then(|i| i.get("content"))
                        .and_then(|x| x.as_str())
                    {
                        output = c.to_string();
                    }
                }
                let mut res = rec("tool_result", output, level, ts);
                res.tool_use_id = call_id.map(String::from);
                if name == "question" {
                    // Feed the dedicated AskUserQuestion card: {question_text: [selected labels]}.
                    res.result_meta = state.and_then(build_question_answers);
                } else if status == "error" {
                    res.result_meta = Some(json!({ "terminal": { "exit_code": 1 } }));
                }
                emit(res);
            }
        }
        "patch" => {
            // The diff content lives in storage/session_diff/<hash>; here we have the file list.
            let files: Vec<String> = p
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let mut r = rec("assistant", String::new(), "tool", ts);
            r.tool_name = Some("patch".to_string());
            r.tool_use_id = p.get("hash").and_then(|h| h.as_str()).map(String::from);
            r.tool_input = Some(json!({ "files": files, "hash": p.get("hash") }));
            emit(r);
        }
        "step-finish" => {
            let tk = p.get("tokens");
            let g = |k: &str| {
                tk.and_then(|t| t.get(k))
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0)
            };
            let cr = tk
                .and_then(|t| t.get("cache"))
                .and_then(|c| c.get("read"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let cw = tk
                .and_then(|t| t.get("cache"))
                .and_then(|c| c.get("write"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            emit(rec(
                "usage",
                format!(
                    "📊 tokens · 输入 {} · 输出 {} · 思考 {} · 缓存读 {} 写 {}",
                    g("input"),
                    g("output"),
                    g("reasoning"),
                    cr,
                    cw
                ),
                "debug",
                ts,
            ));
        }
        "compaction" => {
            emit(rec("meta", "🗜 上下文已压缩".to_string(), "content", ts));
        }
        // step-start and anything else: no record.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_ID: AtomicU64 = AtomicU64::new(0);

    fn page_text(page: &PaginatedRecords) -> Vec<&str> {
        page.records
            .iter()
            .map(|record| record.content_preview.as_str())
            .collect()
    }

    #[test]
    fn clean_read_output_unwraps_content() {
        let wrapped = "<path>/a</path><type>text</type><content>line1\nline2</content>";
        assert_eq!(clean_read_output(wrapped), "line1\nline2");
        assert_eq!(clean_read_output("no wrapper"), "no wrapper");
    }

    #[test]
    fn build_question_answers_pairs_questions_and_answers() {
        let state = json!({
            "input": {"questions":[{"question":"Pick?"}]},
            "metadata": {"answers":[["Yes"]]}
        });
        let meta = build_question_answers(&state).expect("answers");
        assert_eq!(meta["answers"]["Pick?"], json!(["Yes"]));
    }

    #[test]
    fn build_part_text_role_maps_to_speaker() {
        let mut out = Vec::new();
        build_part(
            "assistant",
            &json!({"type":"text","text":"hi"}),
            &None,
            "content",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record_type, "assistant");
        assert_eq!(out[0].content_preview, "hi");
    }

    #[test]
    fn build_part_reasoning_is_thinking() {
        let mut out = Vec::new();
        build_part(
            "assistant",
            &json!({"type":"reasoning","text":"why"}),
            &None,
            "content",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record_type, "thinking");
        assert_eq!(out[0].content_preview, "why");
    }

    #[test]
    fn build_part_tool_splits_into_call_and_result() {
        let mut out = Vec::new();
        let part = json!({
            "type":"tool","tool":"bash","callID":"c1",
            "state":{"status":"completed","input":{"command":"ls"},"output":"done"}
        });
        build_part("assistant", &part, &None, "tool", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].record_type, "assistant");
        assert_eq!(out[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(out[0].tool_use_id.as_deref(), Some("c1"));
        assert_eq!(out[1].record_type, "tool_result");
        assert_eq!(out[1].content_preview, "done");
    }

    #[test]
    fn sqlite_reverse_pages_use_stable_message_cursors_without_overlap() {
        let id = DB_ID.fetch_add(1, Ordering::Relaxed);
        let db = std::env::temp_dir().join(format!(
            "code-dejavu-opencode-{}-{}.db",
            std::process::id(),
            id
        ));
        {
            let conn = Connection::open(&db).expect("create sqlite");
            conn.execute_batch(
                "CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    data TEXT NOT NULL,
                    time_created INTEGER NOT NULL
                 );
                 CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    data TEXT NOT NULL,
                    time_created INTEGER NOT NULL
                 );",
            )
            .expect("schema");
            for index in 0..9 {
                let message_id = format!("m{index:02}");
                conn.execute(
                    "INSERT INTO message (id, session_id, data, time_created) VALUES (?1, 's', ?2, 1)",
                    params![message_id, r#"{"role":"user"}"#],
                )
                .expect("message");
                conn.execute(
                    "INSERT INTO part (id, message_id, session_id, data, time_created) VALUES (?1, ?2, 's', ?3, 1)",
                    params![
                        format!("p{index:02}"),
                        message_id,
                        serde_json::to_string(
                            &json!({"type":"text", "text":format!("msg {index}")})
                        )
                        .expect("part json")
                    ],
                )
                .expect("part");
            }
        }
        let provider = OpenCodeProvider {
            db: db.clone(),
            config_dir: PathBuf::new(),
            data_dir: PathBuf::new(),
            archive_root: PathBuf::new(),
            host: Host::Native,
        };

        let newest = provider
            .session_tail("", "s", 3, "content", None)
            .expect("tail");
        let middle = provider
            .session_before("", "s", newest.start_byte_offset, 3, "content", None)
            .expect("middle");
        let oldest = provider
            .session_before("", "s", middle.start_byte_offset, 3, "content", None)
            .expect("oldest");

        assert_eq!(page_text(&oldest), vec!["msg 0", "msg 1", "msg 2"]);
        assert_eq!(page_text(&middle), vec!["msg 3", "msg 4", "msg 5"]);
        assert_eq!(page_text(&newest), vec!["msg 6", "msg 7", "msg 8"]);
        assert_eq!(oldest.start_byte_offset, 0);
        assert_eq!(middle.start_byte_offset, 3);
        assert_eq!(newest.start_byte_offset, 6);
        assert!(!oldest.has_earlier);
        assert!(middle.has_earlier);
        assert!(newest.has_earlier);

        drop(provider);
        let _ = fs::remove_file(db);
    }
}
