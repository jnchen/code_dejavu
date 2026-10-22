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

/// Which on-disk schema this database uses.
///
/// OpenCode v1 keeps `session`/`message`/`part`; OpenCode 2 projects sessions into
/// `session_v2`/`session_message`. A v1 install upgraded to v2 can keep both sets of tables, and
/// users are not forced to upgrade, so both are read: v2 by preference, v1 for every session that
/// has not been migrated (or when the user never upgraded at all).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreSchema {
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, Default)]
struct StoreTables {
    v1: bool,
    v2: bool,
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        != 0
}

fn store_tables(conn: &Connection) -> StoreTables {
    StoreTables {
        // Some v1 fixtures/older stores expose only the message tables; either shape is readable.
        v1: table_exists(conn, "session") || table_exists(conn, "message"),
        v2: table_exists(conn, "session_v2") && table_exists(conn, "session_message"),
    }
}

/// Resolve one session to the store that actually holds it. This is deliberately per-session (not
/// per-database): an upgraded install can contain both v2 projections and v1 sessions the
/// migration skipped, and a v1-only install must keep working unchanged.
fn session_schema(conn: &Connection, session_id: &str) -> StoreSchema {
    let tables = store_tables(conn);
    if tables.v2 {
        let found = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session_v2 WHERE id=?1)",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0);
        if found != 0 {
            return StoreSchema::V2;
        }
    }
    if tables.v1 {
        return StoreSchema::V1;
    }
    StoreSchema::V2
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
        let schema = store_tables(&conn);
        if schema.v1 {
            Self::push_roots_from_query(
                &conn,
                &self.host,
                &mut roots,
                &mut seen,
                "SELECT directory FROM session WHERE directory IS NOT NULL AND directory <> ''",
            );
            // The v1 table used `id` (a project path) and an optional `path` column. These
            // statements are allowed to fail on schemas that never had them.
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
                "SELECT path FROM project_directory WHERE path IS NOT NULL AND path <> ''",
            );
        }
        if schema.v2 {
            Self::push_roots_from_query(
                &conn,
                &self.host,
                &mut roots,
                &mut seen,
                "SELECT directory FROM session_v2 WHERE directory IS NOT NULL AND directory <> ''",
            );
            Self::push_roots_from_query(
                &conn,
                &self.host,
                &mut roots,
                &mut seen,
                "SELECT directory FROM worktree WHERE directory IS NOT NULL AND directory <> ''",
            );
        }
        Self::push_roots_from_query(
            &conn,
            &self.host,
            &mut roots,
            &mut seen,
            "SELECT worktree FROM project WHERE worktree IS NOT NULL AND worktree <> ''",
        );
        Self::push_roots_from_query(
            &conn,
            &self.host,
            &mut roots,
            &mut seen,
            "SELECT directory FROM project_directory WHERE directory IS NOT NULL AND directory <> ''",
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
        let tables = store_tables(conn);
        let mut sessions = Vec::new();
        if tables.v2 {
            match Self::list_sessions_v2(conn, host, project, archive_name.clone()) {
                Ok(v2) => sessions = v2,
                // A v1-only install never reaches this. On an upgraded install a broken/incomplete
                // v2 projection must not take the intact v1 history down with it.
                Err(error) if !tables.v1 => return Err(error),
                Err(_) => {}
            }
        }
        if tables.v1 {
            let seen: std::collections::HashSet<String> =
                sessions.iter().map(|s| s.session_id.clone()).collect();
            let mut v1 = Self::list_sessions_v1(conn, host, project, archive_name)?;
            v1.retain(|session| !seen.contains(&session.session_id));
            sessions.extend(v1);
        }
        sessions.sort_by(|a, b| {
            b.timestamp
                .as_deref()
                .unwrap_or("")
                .cmp(a.timestamp.as_deref().unwrap_or(""))
        });
        Ok(sessions)
    }

    fn list_sessions_v1(
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

    /// OpenCode 2 stores sessions in `session_v2` and messages in `session_message`. Titles are
    /// nullable there, so a session without a generated title is still listed.
    fn list_sessions_v2(
        conn: &Connection,
        host: &Host,
        project: Option<&str>,
        archive_name: Option<String>,
    ) -> Result<Vec<SessionSummary>, AppError> {
        let map_count = |sql: &str| -> HashMap<String, i64> {
            let mut map = HashMap::new();
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                {
                    for row in rows.flatten() {
                        map.insert(row.0, row.1);
                    }
                }
            }
            map
        };
        let msg_counts =
            map_count("SELECT session_id, count(*) FROM session_message GROUP BY session_id");
        let child_counts = map_count(
            "SELECT parent_id, count(*) FROM session_v2 \
             WHERE parent_id IS NOT NULL GROUP BY parent_id",
        );
        let model_contexts = Self::model_contexts_by_top_v2(conn);

        let mut stmt = conn
            .prepare(
                "SELECT id, directory, title, time_created, time_updated \
                 FROM session_v2 WHERE parent_id IS NULL ORDER BY time_updated DESC",
            )
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
            if let Some(expected) = project {
                if dir != expected {
                    continue;
                }
            }
            let record_count = *msg_counts.get(&id).unwrap_or(&0) as u32;
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
                file_size_bytes: (record_count as u64) * 400,
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
        let tables = store_tables(conn);
        let mut docs = if tables.v2 {
            self.index_documents_v2(conn, archive_name.clone())
        } else {
            Vec::new()
        };
        if tables.v1 {
            let seen: std::collections::HashSet<String> =
                docs.iter().map(|doc| doc.session_id.clone()).collect();
            let mut v1 = self.index_documents_v1(conn, archive_name);
            v1.retain(|doc| !seen.contains(&doc.session_id));
            docs.extend(v1);
        }
        docs
    }

    fn index_documents_v1(&self, conn: &Connection, archive_name: Option<String>) -> Vec<IndexDoc> {
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
        for s in Self::list_sessions_v1(conn, &self.host, None, archive_name).unwrap_or_default() {
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

    /// Build the same bounded search documents from the v2 projection. Each `session_message` row
    /// is already a complete message (user text, assistant content blocks, or a shell result), so
    /// no part join is needed.
    fn index_documents_v2(&self, conn: &Connection, archive_name: Option<String>) -> Vec<IndexDoc> {
        let mut parent: HashMap<String, Option<String>> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT id, parent_id FROM session_v2") {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            }) {
                for row in rows.flatten() {
                    parent.insert(row.0, row.1);
                }
            }
        }
        let top_of = |id: String| top_session(&parent, id);

        let mut by_top: HashMap<String, FastIndexTextCollector> = HashMap::new();
        let mut first_by_top: HashMap<String, String> = HashMap::new();
        let mut tokens_by_top: HashMap<String, TokenUsage> = HashMap::new();

        if let Ok(mut stmt) =
            conn.prepare("SELECT session_id, type, data FROM session_message ORDER BY seq")
        {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    let (sid, message_type, data) = row;
                    let Ok(value) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    let top = top_of(sid.clone());
                    let is_top = sid == top;
                    let mut texts: Vec<(&str, String)> = Vec::new();
                    match message_type.as_str() {
                        "user" => {
                            if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                                if !text.trim().is_empty() {
                                    if is_top {
                                        first_by_top
                                            .entry(top.clone())
                                            .or_insert_with(|| text.chars().take(200).collect());
                                    }
                                    texts.push(("content", text.to_string()));
                                }
                            }
                        }
                        "assistant" => {
                            if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
                                for item in content {
                                    match item.get("type").and_then(|v| v.as_str()) {
                                        Some("text") => {
                                            if let Some(text) =
                                                item.get("text").and_then(|v| v.as_str())
                                            {
                                                texts.push(("content", text.to_string()));
                                            }
                                        }
                                        Some("reasoning") => {
                                            if let Some(text) =
                                                item.get("text").and_then(|v| v.as_str())
                                            {
                                                texts.push(("reasoning", text.to_string()));
                                            }
                                        }
                                        Some("tool") => {
                                            let input = item
                                                .pointer("/state/input")
                                                .map(|v| v.to_string())
                                                .unwrap_or_default();
                                            let output = v2_tool_output(item);
                                            texts.push(("tool", format!("{} {}", input, output)));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        "synthetic" | "system" | "skill" => {
                            if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                                texts.push(("content", text.to_string()));
                            }
                        }
                        "shell" => {
                            let command =
                                value.get("command").and_then(|v| v.as_str()).unwrap_or("");
                            let output = v2_shell_output(value.get("output"));
                            texts.push(("tool", format!("{} {}", command, output)));
                        }
                        _ => {}
                    }
                    let collector = by_top.entry(top).or_default();
                    for (kind, text) in texts {
                        if !text.trim().is_empty() {
                            collector.push(kind, text);
                        }
                    }
                }
            }
        }

        // Token totals live on each session row and already include reasoning and cache.
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, tokens_input, tokens_output, tokens_reasoning, \
             tokens_cache_read, tokens_cache_write FROM session_v2",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            }) {
                for row in rows.flatten() {
                    let (sid, input, output, reasoning, cache_read, cache_write) = row;
                    let top = top_of(sid);
                    let entry = tokens_by_top.entry(top).or_default();
                    let output = output + reasoning;
                    let cache = cache_read + cache_write;
                    entry.input_tokens += input.max(0) as u64;
                    entry.output_tokens += output.max(0) as u64;
                    entry.cache_tokens += cache.max(0) as u64;
                    entry.total_tokens += (input + output + cache).max(0) as u64;
                }
            }
        }

        let mut docs = Vec::new();
        for s in Self::list_sessions_v2(conn, &self.host, None, archive_name).unwrap_or_default() {
            let texts = by_top
                .remove(&s.session_id)
                .unwrap_or_default()
                .into_texts();
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

    fn model_contexts_by_top_v2(conn: &Connection) -> HashMap<String, Vec<SessionModelInfo>> {
        let mut parent: HashMap<String, Option<String>> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT id, parent_id FROM session_v2") {
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
            conn.prepare("SELECT id, model FROM session_v2 WHERE model IS NOT NULL AND model <> ''")
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
            "SELECT session_id, type, data FROM session_message \
             WHERE type IN ('model-switched', 'assistant')",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    let (sid, message_type, data) = row;
                    let Ok(value) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    let top = top_session(&parent, sid);
                    let contexts = contexts.entry(top).or_default();
                    if let Some(model) = value.get("model") {
                        push_opencode_model_value(contexts, model);
                    }
                    if message_type == "assistant" {
                        if let Some(model_id) = value.get("modelID").and_then(|v| v.as_str()) {
                            push_model_context(
                                contexts,
                                value_str(&value, "providerID"),
                                Some(model_id.to_string()),
                                value
                                    .get("variant")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            );
                        }
                    }
                }
            }
        }

        contexts
    }

    fn message_count(conn: &Connection, session_id: &str) -> i64 {
        let table = match session_schema(conn, session_id) {
            StoreSchema::V1 => "message",
            StoreSchema::V2 => "session_message",
        };
        conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE session_id=?1"),
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
        match session_schema(conn, session_id) {
            StoreSchema::V1 => Self::message_batches_v1(conn, session_id, start, end, min_level),
            StoreSchema::V2 => Self::message_batches_v2(conn, session_id, start, end, min_level),
        }
    }

    fn message_batches_v1(
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

    fn message_batches_v2(
        conn: &Connection,
        session_id: &str,
        start: i64,
        end: i64,
        min_level: &str,
    ) -> Result<Vec<OpenCodeMessageBatch>, AppError> {
        if start >= end {
            return Ok(Vec::new());
        }
        let mut stmt = conn
            .prepare(
                "SELECT id, type, data, time_created FROM session_message \
                 WHERE session_id=?1 ORDER BY seq LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let rows = stmt
            .query_map(params![session_id, end - start, start], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let messages: Vec<(String, String, String, i64)> = rows
            .collect::<Result<_, _>>()
            .map_err(|e| AppError::Archive(e.to_string()))?;

        let mut batches = Vec::new();
        for (position, (message_id, message_type, data, created)) in
            messages.into_iter().enumerate()
        {
            let cursor = start as u64 + position as u64;
            let timestamp = Some(fmt_ms_full(created));
            let mut records = Vec::new();
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                build_v2_message(
                    &message_type,
                    &value,
                    &message_id,
                    &timestamp,
                    min_level,
                    &mut records,
                );
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

    fn list_subagents_v1(
        conn: &Connection,
        session_id: &str,
    ) -> Result<Vec<SubagentInfo>, AppError> {
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
                parent_agent_id: None,
            });
        }
        Ok(out)
    }

    /// OpenCode 2 supports nested subagents (up to `experimental.subagent_depth`). Collect every
    /// descendant session, not only direct children, so a grandchild's `subagent` tool call can
    /// still find its transcript while the nested timeline renders recursively.
    fn list_subagents_v2(
        conn: &Connection,
        session_id: &str,
    ) -> Result<Vec<SubagentInfo>, AppError> {
        let mut kids: Vec<(String, Option<String>, String, String)> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "WITH RECURSIVE descendants(id) AS ( \
                         SELECT id FROM session_v2 WHERE parent_id=?1 \
                         UNION \
                         SELECT s.id FROM session_v2 s JOIN descendants d ON s.parent_id=d.id \
                     ) \
                     SELECT s.id, s.parent_id, coalesce(s.title,''), coalesce(s.agent,'') \
                     FROM session_v2 s JOIN descendants d ON d.id=s.id \
                     ORDER BY s.time_created",
                )
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = stmt
                .query_map([session_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
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

        let mut child_agents: HashMap<String, String> = HashMap::new();
        for (id, _parent, _title, agent) in &kids {
            if !agent.is_empty() {
                child_agents.insert(id.clone(), agent.clone());
            }
        }

        // Map child_session_id -> (tool call id, agent type) from every session that can contain
        // the spawning call (the root and any intermediate child).
        let mut link: HashMap<String, (String, String)> = HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, data FROM session_message \
                     WHERE (session_id=?1 OR session_id IN (SELECT id FROM session_v2 WHERE parent_id IS NOT NULL)) \
                       AND type='assistant'",
                )
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = stmt
                .query_map([session_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let known: std::collections::HashSet<&str> =
                kids.iter().map(|(id, _, _, _)| id.as_str()).collect();
            for (sid, data) in rows.flatten() {
                if sid != session_id && !known.contains(sid.as_str()) {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                let Some(content) = value.get("content").and_then(|v| v.as_array()) else {
                    continue;
                };
                for item in content {
                    if item.get("type").and_then(|v| v.as_str()) != Some("tool") {
                        continue;
                    }
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name != "subagent" && name != "task" {
                        continue;
                    }
                    let call_id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let state = item.get("state");
                    let metadata = state.and_then(|s| s.get("metadata"));
                    let child = metadata
                        .and_then(|m| m.get("sessionID").or_else(|| m.get("sessionId")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if child.is_empty() {
                        continue;
                    }
                    let agent = metadata
                        .and_then(|m| m.get("agent"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .or_else(|| {
                            state
                                .and_then(|s| s.get("input"))
                                .and_then(|i| i.get("agent").or_else(|| i.get("subagent_type")))
                                .and_then(|v| v.as_str())
                                .map(String::from)
                        })
                        .or_else(|| child_agents.get(child).cloned())
                        .unwrap_or_else(|| "subagent".to_string());
                    link.insert(child.to_string(), (call_id, agent));
                }
            }
        }

        let mut counts: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT session_id, count(*) FROM session_message GROUP BY session_id")
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| AppError::Archive(e.to_string()))?;
            for row in rows.flatten() {
                counts.insert(row.0, row.1);
            }
        }

        let mut out = Vec::new();
        for (id, parent_id, title, child_agent) in kids {
            let (tool_use_id, agent_type) = link.get(&id).cloned().unwrap_or_default();
            let agent_type = if !agent_type.is_empty() {
                agent_type
            } else if !child_agent.is_empty() {
                child_agent
            } else {
                "subagent".to_string()
            };
            out.push(SubagentInfo {
                agent_id: id.clone(),
                agent_type,
                description: title,
                tool_use_id,
                record_count: *counts.get(&id).unwrap_or(&0) as u32,
                // Direct children spawn from the requested session; a nested agent reports its
                // immediate parent agent so the UI can indent it if it ever wants to.
                parent_agent_id: parent_id.filter(|parent| parent != session_id),
            });
        }
        Ok(out)
    }

    fn search_in_session_v1(
        conn: &Connection,
        session_id: &str,
        query: &str,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
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

    fn search_in_session_v2(
        conn: &Connection,
        session_id: &str,
        query: &str,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let mut order: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT id FROM session_message WHERE session_id=?1 ORDER BY seq")
                .map_err(|e| AppError::Archive(e.to_string()))?;
            let rows = stmt
                .query_map([session_id], |r| r.get::<_, String>(0))
                .map_err(|e| AppError::Archive(e.to_string()))?;
            for row in rows.flatten() {
                order.push(row);
            }
        }
        let index_of: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();

        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT id, type, data, time_created FROM session_message \
                 WHERE session_id=?1 ORDER BY seq",
            )
            .map_err(|e| AppError::Archive(e.to_string()))?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| AppError::Archive(e.to_string()))?;
        for row in rows.flatten() {
            let (mid, message_type, data, created) = row;
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let mut candidates: Vec<(&str, String)> = Vec::new();
            match message_type.as_str() {
                "user" | "synthetic" | "system" => {
                    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                        candidates.push(("message", text.to_string()));
                    }
                }
                "assistant" => {
                    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
                        for item in content {
                            match item.get("type").and_then(|v| v.as_str()) {
                                Some("text") => {
                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                        candidates.push(("message", text.to_string()));
                                    }
                                }
                                Some("reasoning") => {
                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                        candidates.push(("thinking", text.to_string()));
                                    }
                                }
                                Some("tool") => {
                                    let input = item
                                        .pointer("/state/input")
                                        .map(|v| v.to_string())
                                        .unwrap_or_default();
                                    candidates.push((
                                        "tool_result",
                                        format!("{} {}", input, v2_tool_output(item)),
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "shell" => {
                    let command = value.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    let output = v2_shell_output(value.get("output"));
                    candidates.push(("tool_result", format!("{} {}", command, output)));
                }
                _ => {}
            }
            for (record_type, text) in candidates {
                if text.is_empty() || !text.to_lowercase().contains(&needle) {
                    continue;
                }
                let timestamp = chrono::DateTime::from_timestamp_millis(created).map(|dt| {
                    let local: chrono::DateTime<chrono::Local> = dt.into();
                    local.format("%Y-%m-%d %H:%M:%S").to_string()
                });
                hits.push(SessionSearchHit {
                    byte_offset: *index_of.get(mid.as_str()).unwrap_or(&0) as u64,
                    snippet: crate::services::search::extract_snippet(&text, &needle),
                    record_type: record_type.to_string(),
                    timestamp,
                });
            }
        }
        Ok(hits)
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
        if session_schema(&conn, session_id) == StoreSchema::V2 {
            let mut stmt = conn
                .prepare(
                    "SELECT data FROM session_message \
                     WHERE session_id=?1 AND type='user' ORDER BY seq LIMIT 20",
                )
                .ok()?;
            let rows = stmt
                .query_map([session_id], |r| r.get::<_, String>(0))
                .ok()?;
            for data in rows.flatten() {
                let Ok(message) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if let Some(text) = message.get("text").and_then(|v| v.as_str()) {
                    let text = text.trim();
                    if !text.is_empty() {
                        return Some(text.chars().take(200).collect());
                    }
                }
            }
            return None;
        }
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
        match session_schema(&conn, session_id) {
            StoreSchema::V1 => Self::list_subagents_v1(&conn, session_id),
            StoreSchema::V2 => {
                let mut agents = Self::list_subagents_v2(&conn, session_id)?;
                // A migration may leave a child behind in the v1 tables. Keep it visible as a
                // child of the v2 parent rather than hiding it.
                if store_tables(&conn).v1 {
                    let seen: std::collections::HashSet<String> =
                        agents.iter().map(|agent| agent.agent_id.clone()).collect();
                    let mut legacy = Self::list_subagents_v1(&conn, session_id)?;
                    legacy.retain(|agent| !seen.contains(&agent.agent_id));
                    agents.extend(legacy);
                }
                Ok(agents)
            }
        }
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
        match session_schema(&conn, session_id) {
            StoreSchema::V1 => Self::search_in_session_v1(&conn, session_id, query),
            StoreSchema::V2 => Self::search_in_session_v2(&conn, session_id, query),
        }
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

/// Flatten a v2 tool result's `content` blocks into display text. File blocks keep a visible
/// placeholder so an attachment-only result is not rendered as an empty card.
fn v2_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => block.get("text").and_then(|v| v.as_str()).map(String::from),
                Some("file") => {
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| block.get("uri").and_then(|v| v.as_str()))
                        .unwrap_or("file");
                    Some(format!("[file: {name}]"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn v2_shell_output(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Array(_) => v2_content_text(Some(value)),
        Value::Object(_) => value
            .get("output")
            .or_else(|| value.get("text"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
}

fn v2_tool_output(item: &Value) -> String {
    let state = item.get("state");
    let content = v2_content_text(state.and_then(|s| s.get("content")));
    if !content.is_empty() {
        return content;
    }
    if let Some(output) = state.and_then(|s| s.get("output")) {
        return match output {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
    }
    state
        .and_then(|s| s.get("error"))
        .map(|error| {
            error
                .get("message")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| error.to_string())
        })
        .unwrap_or_default()
}

fn v2_tool_timestamp(item: &Value, fallback: &Option<String>) -> Option<String> {
    item.pointer("/time/created")
        .or_else(|| item.pointer("/time/start"))
        .and_then(|v| v.as_i64())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|dt| {
            let local: chrono::DateTime<chrono::Local> = dt.into();
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .or_else(|| fallback.clone())
}

/// Build the display records for one v2 `session_message` row. A v2 assistant message already
/// carries its text/reasoning/tool blocks together, so a single row can become several records.
fn build_v2_message(
    message_type: &str,
    value: &Value,
    message_id: &str,
    ts: &Option<String>,
    min_level: &str,
    out: &mut Vec<SessionRecord>,
) {
    match message_type {
        "user" => {
            let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !text.trim().is_empty() {
                let r = rec("user", text.to_string(), "content", ts);
                if keep(&r.level, min_level) {
                    out.push(r);
                }
            }
        }
        "synthetic" | "system" => {
            let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !text.trim().is_empty() {
                let r = rec("meta", text.to_string(), "content", ts);
                if keep(&r.level, min_level) {
                    out.push(r);
                }
            }
        }
        "assistant" => {
            let Some(content) = value.get("content").and_then(|v| v.as_array()) else {
                return;
            };
            for item in content {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.trim().is_empty() {
                            let mut r = rec("assistant", text.to_string(), "content", ts);
                            r.group_id = Some(message_id.to_string());
                            if keep(&r.level, min_level) {
                                out.push(r);
                            }
                        }
                    }
                    Some("reasoning") => {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.trim().is_empty() {
                            let mut r = rec("thinking", text.to_string(), "content", ts);
                            r.group_id = Some(message_id.to_string());
                            if keep(&r.level, min_level) {
                                out.push(r);
                            }
                        }
                    }
                    Some("tool") => {
                        build_v2_tool(item, message_id, ts, min_level, out);
                    }
                    _ => {}
                }
            }
        }
        "shell" => {
            let command = value.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let call_id = value.get("shellID").and_then(|v| v.as_str());
            let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let level = "tool";
            let mut call = rec("assistant", String::new(), level, ts);
            call.tool_name = Some("shell".to_string());
            call.tool_use_id = call_id.map(String::from);
            call.tool_input = Some(json!({ "command": command }));
            call.group_id = Some(message_id.to_string());
            if keep(&call.level, min_level) {
                out.push(call);
            }
            if status != "running" {
                let output = v2_shell_output(value.get("output"));
                let mut result = rec("tool_result", output, level, ts);
                result.tool_use_id = call_id.map(String::from);
                if let Some(exit) = value.get("exit").and_then(|v| v.as_i64()) {
                    if exit != 0 {
                        result.result_meta = Some(json!({ "terminal": { "exit_code": exit } }));
                    }
                }
                if keep(&result.level, min_level) {
                    out.push(result);
                }
            }
        }
        "compaction" => {
            let summary = value.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let content = if summary.trim().is_empty() {
                "🗜 上下文已压缩".to_string()
            } else {
                format!("🗜 上下文已压缩\n\n{summary}")
            };
            let r = rec("meta", content, "content", ts);
            if keep(&r.level, min_level) {
                out.push(r);
            }
        }
        "skill" => {
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("skill");
            let r = rec("meta", format!("⚡ {name}"), "content", ts);
            if keep(&r.level, min_level) {
                out.push(r);
            }
        }
        _ => {}
    }
}

fn build_v2_tool(
    item: &Value,
    message_id: &str,
    ts: &Option<String>,
    min_level: &str,
    out: &mut Vec<SessionRecord>,
) {
    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
    let call_id = item.get("id").and_then(|v| v.as_str());
    let state = item.get("state");
    let status = state
        .and_then(|s| s.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // v2 calls its subagent tool `subagent`; reuse the Agent branch and the same nested timeline.
    let display_name = if name == "subagent" || name == "task" {
        "Agent"
    } else {
        name
    };
    let level = if name == "question" {
        "content"
    } else {
        "tool"
    };
    let timestamp = v2_tool_timestamp(item, ts);

    let mut input = state.and_then(|s| s.get("input")).cloned();
    if let Some(Value::Object(ref mut map)) = input {
        // OpenCode 2 names the file argument `path`; the file cards bind to `file_path`.
        if matches!(name, "read" | "write" | "edit") {
            if let Some(path) = map.remove("path") {
                map.entry("file_path".to_string()).or_insert(path);
            }
        }
    }
    let mut call = rec("assistant", String::new(), level, &timestamp);
    call.tool_name = Some(display_name.to_string());
    call.tool_use_id = call_id.map(String::from);
    call.tool_input = input;
    call.group_id = Some(message_id.to_string());
    if keep(&call.level, min_level) {
        out.push(call);
    }

    if status == "streaming" || status == "running" || status.is_empty() {
        return;
    }

    let mut output = v2_tool_output(item);
    if name == "write" {
        if let Some(content) = state
            .and_then(|s| s.get("input"))
            .and_then(|i| i.get("content"))
            .and_then(|v| v.as_str())
        {
            output = content.to_string();
        }
    }
    let mut result = rec("tool_result", output, level, &timestamp);
    result.tool_use_id = call_id.map(String::from);
    if name == "question" {
        result.result_meta = state.and_then(build_question_answers);
    } else if status == "error" {
        result.result_meta = Some(json!({ "terminal": { "exit_code": 1 } }));
    }
    if keep(&result.level, min_level) {
        out.push(result);
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

    fn v2_provider(db: PathBuf) -> OpenCodeProvider {
        OpenCodeProvider {
            db,
            config_dir: PathBuf::new(),
            data_dir: PathBuf::new(),
            archive_root: PathBuf::new(),
            host: Host::Native,
        }
    }

    fn create_v2_db() -> PathBuf {
        let id = DB_ID.fetch_add(1, Ordering::Relaxed);
        let db = std::env::temp_dir().join(format!(
            "code-dejavu-opencode-v2-{}-{}.db",
            std::process::id(),
            id
        ));
        let conn = Connection::open(&db).expect("create sqlite");
        conn.execute_batch(
            "CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY,
                project_id TEXT,
                parent_id TEXT,
                directory TEXT,
                title TEXT,
                version TEXT,
                agent TEXT,
                model TEXT,
                tokens_input INTEGER NOT NULL DEFAULT 0,
                tokens_output INTEGER NOT NULL DEFAULT 0,
                tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
             );
             CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                seq INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
             );
             -- A v2 upgrade can leave the v1 tables in place; sessions the migration skipped (and
             -- users who never upgraded) must still be readable.
             CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                directory TEXT,
                title TEXT,
                model TEXT,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
             );
             CREATE TABLE message (
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
        let session = |id: &str, parent: Option<&str>, title: &str, agent: &str, created: i64| {
            conn.execute(
                "INSERT INTO session_v2 (id, parent_id, directory, title, version, agent, time_created, time_updated) \
                 VALUES (?1, ?2, 'C:/project', ?3, '2.1.2', ?4, ?5, ?5)",
                params![id, parent, title, agent, created],
            )
            .expect("session");
        };
        session("root", None, "root", "build", 1);
        session("child", Some("root"), "child task", "general", 2);
        session("grandchild", Some("child"), "grandchild task", "explore", 3);
        let message = |id: &str, sid: &str, mtype: &str, seq: i64, data: Value| {
            conn.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                params![
                    id,
                    sid,
                    mtype,
                    seq,
                    1_700_000_000_000_i64 + seq,
                    data.to_string()
                ],
            )
            .expect("message");
        };
        message(
            "m1",
            "root",
            "user",
            0,
            json!({"text":"start", "time":{"created":1}}),
        );
        message(
            "m2",
            "root",
            "assistant",
            1,
            json!({"content":[
                {"type":"text","text":"I'll delegate."},
                {"type":"tool","id":"tool-1","name":"subagent","state":{
                    "status":"completed",
                    "input":{"agent":"general","description":"child task"},
                    "content":[{"type":"text","text":"done"}],
                    "metadata":{"sessionID":"child","agent":"general"}
                }}
            ]}),
        );
        message(
            "m3",
            "child",
            "assistant",
            0,
            json!({"content":[
                {"type":"tool","id":"tool-2","name":"subagent","state":{
                    "status":"completed",
                    "input":{"agent":"explore"},
                    "content":[{"type":"text","text":"nested"}],
                    "metadata":{"sessionID":"grandchild","agent":"explore"}
                }}
            ]}),
        );
        message(
            "m4",
            "grandchild",
            "assistant",
            0,
            json!({"content":[
                {"type":"text","text":"leaf"},
                {"type":"tool","id":"tool-3","name":"read","state":{
                    "status":"completed",
                    "input":{"path":"src/lib.rs"},
                    "content":[{"type":"text","text":"file body"}]
                }}
            ]}),
        );
        // A migrated session left in the v1 tables must not win over its v2 projection.
        conn.execute(
            "INSERT INTO session (id, directory, title, time_created, time_updated) \
             VALUES ('root', 'C:/project', 'stale v1', 10, 10)",
            [],
        )
        .expect("v1 duplicate session");
        conn.execute(
            "INSERT INTO message (id, session_id, data, time_created) \
             VALUES ('v1-root-msg', 'root', '{\"role\":\"user\"}', 10)",
            [],
        )
        .expect("v1 duplicate message");
        // A session present only in v1 (not yet migrated, or a v1-only install).
        conn.execute(
            "INSERT INTO session (id, directory, title, time_created, time_updated) \
             VALUES ('v1only', 'C:/project', 'old v1 only', 20, 20)",
            [],
        )
        .expect("v1 session");
        conn.execute(
            "INSERT INTO message (id, session_id, data, time_created) \
             VALUES ('v1-msg', 'v1only', '{\"role\":\"user\"}', 20)",
            [],
        )
        .expect("v1 message");
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, data, time_created) \
             VALUES ('v1-part', 'v1-msg', 'v1only', '{\"type\":\"text\",\"text\":\"v1 legacy\"}', 20)",
            [],
        )
        .expect("v1 part");
        drop(conn);
        db
    }

    #[test]
    fn v2_sessions_messages_and_nested_subagents_are_parsed() {
        let db = create_v2_db();
        let provider = v2_provider(db.clone());

        let sessions = provider.list_sessions(None).expect("sessions");
        assert_eq!(sessions.len(), 2);
        let root = sessions
            .iter()
            .find(|s| s.session_id == "root")
            .expect("v2 root");
        assert_eq!(root.subagent_count, 1);
        assert_eq!(root.project, "C:/project");
        assert_eq!(root.agent_title.as_deref(), Some("root"));
        assert!(sessions.iter().any(|s| s.session_id == "v1only"));

        let detail = provider
            .session_detail("", "root", 0, 100, "tool", None)
            .expect("detail");
        assert_eq!(detail.records[0].record_type, "user");
        assert_eq!(detail.records[0].content_preview, "start");
        assert!(detail
            .records
            .iter()
            .any(|r| r.tool_name.as_deref() == Some("Agent")
                && r.tool_use_id.as_deref() == Some("tool-1")));

        // Sessions that exist only in the v1 tables still render through the v1 parser.
        let legacy = provider
            .session_detail("", "v1only", 0, 100, "content", None)
            .expect("v1 detail");
        assert_eq!(legacy.records[0].content_preview, "v1 legacy");

        let mut agents = provider
            .list_subagents("", "root", None)
            .expect("subagents");
        agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        assert_eq!(agents.len(), 2);
        let grandchild = agents
            .iter()
            .find(|a| a.agent_id == "grandchild")
            .expect("nested agent");
        assert_eq!(grandchild.agent_type, "explore");
        assert_eq!(grandchild.parent_agent_id.as_deref(), Some("child"));
        assert_eq!(grandchild.tool_use_id, "tool-2");

        let leaf = provider
            .session_detail("", "grandchild", 0, 100, "tool", None)
            .expect("nested detail");
        let read = leaf
            .records
            .iter()
            .find(|r| r.tool_name.as_deref() == Some("read"))
            .expect("read call");
        assert_eq!(
            read.tool_input.as_ref().and_then(|v| v.get("file_path")),
            Some(&json!("src/lib.rs"))
        );

        drop(provider);
        let _ = fs::remove_file(db);
    }
}
