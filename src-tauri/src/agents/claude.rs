//! Claude Code provider — `~/.claude`, per-project session dirs, slug-encoded paths.

use super::{
    metadata_pool, quote_command_arg, AgentProvider, Capabilities, FastIndexTextCollector,
    IndexBatch, IndexDoc, IndexManifestEntry, IndexText, InstructionCandidate, TokenUsage,
    WorkflowItem,
};
use crate::error::AppError;
use crate::hosts::Host;
use crate::models::memory::{MemoryFile, MemoryFrontmatter, ProjectInfo};
use crate::models::profile::ProfileArchive;
use crate::models::rule::{RuleFile, RuleFrontmatter};
use crate::models::session::{
    push_model_context, PaginatedRecords, SessionModelInfo, SessionSearchHit, SessionSummary,
    SubagentInfo,
};
use crate::paths::ClaudePaths;
use crate::services::{claude_archiver, claude_scanner, frontmatter, jsonl};
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Short description for a workflow card: the frontmatter `description:` if present, else the first
/// non-empty, non-heading line of the body. Truncated so list cards stay compact.
fn workflow_description(content: &str) -> String {
    let (yaml, body) = frontmatter::split_frontmatter(content);
    if let Some(y) = yaml {
        for line in y.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("description:") {
                let d = rest.trim().trim_matches(['"', '\'']).trim();
                if !d.is_empty() {
                    return truncate_desc(d);
                }
            }
        }
    }
    for line in body.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        return truncate_desc(l);
    }
    String::new()
}

fn truncate_desc(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Cheap content version for incremental indexing: file size + mtime (seconds). Changes whenever
/// a session file is appended to or rewritten, so the engine knows to re-parse just that file.
fn file_version(meta: &fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}:{}", meta.len(), mtime)
}

pub struct ClaudeProvider {
    paths: ClaudePaths,
    /// Which machine this install lives on. Everything host-dependent — slug encoding, and turning
    /// a recorded `cwd` into a path this process can open — goes through it, so a WSL install is
    /// read correctly by a Windows build without any of it becoming a compile-time `cfg`.
    host: Host,
    project_roots_cache: Mutex<Option<(Instant, Vec<PathBuf>)>>,
}

impl ClaudeProvider {
    pub fn new(paths: ClaudePaths) -> Self {
        Self::for_host(Host::Native, paths)
    }

    pub fn for_host(host: Host, paths: ClaudePaths) -> Self {
        Self {
            paths,
            host,
            project_roots_cache: Mutex::new(None),
        }
    }

    /// Project root for a request: the live `projects/` dir, or an archive's `projects/`.
    fn base(&self, archive: Option<&str>) -> PathBuf {
        match archive {
            Some(name) => self.paths.archive_root.join(name).join("projects"),
            None => self.paths.projects_dir.clone(),
        }
    }

    fn session_file(&self, project: &str, session_id: &str, archive: Option<&str>) -> PathBuf {
        self.base(archive)
            .join(project)
            .join(format!("{}.jsonl", session_id))
    }

    /// The project directory a session ran in, as a path *this* process can open: a session
    /// recorded inside WSL stores `/home/…`, which only resolves through the distro's UNC share.
    fn read_session_cwd(path: &Path, host: &Host) -> Option<PathBuf> {
        let file = fs::File::open(path).ok()?;
        let reader = BufReader::with_capacity(32 * 1024, file);
        for line in reader.lines().map_while(Result::ok).take(80) {
            if !line.contains("\"cwd\"") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(cwd) = value.get("cwd").and_then(|cwd| cwd.as_str()) else {
                continue;
            };
            let path = host.to_readable(cwd);
            if path.is_absolute() {
                return Some(path);
            }
        }
        None
    }

    fn live_session_entries(&self) -> Vec<(PathBuf, String)> {
        if !self.paths.projects_dir.exists() {
            return Vec::new();
        }
        fs::read_dir(&self.paths.projects_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .flat_map(|project_entry| {
                let slug = project_entry.file_name().to_string_lossy().to_string();
                fs::read_dir(project_entry.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| {
                        entry.path().extension().is_some_and(|ext| ext == "jsonl")
                            && entry.path().is_file()
                    })
                    .map(move |entry| (entry.path(), slug.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn scan_instruction_project_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = metadata_pool().install(|| {
            self.live_session_entries()
                .into_par_iter()
                .filter_map(|(path, slug)| {
                    let root = Self::read_session_cwd(&path, &self.host)
                        .filter(|cwd| cwd.exists())
                        .unwrap_or_else(|| PathBuf::from(self.host.decode_project_slug(&slug)));
                    (!root.as_os_str().is_empty()).then_some(root)
                })
                .collect()
        });
        roots.sort();
        roots.dedup();
        roots
    }

    fn add_project_rule_file(
        &self,
        rules: &mut Vec<RuleFile>,
        project_path: &Path,
        filename: &str,
    ) -> Result<(), AppError> {
        let path = project_path.join(filename);
        if !path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&path)?;
        let size_bytes = fs::metadata(&path)?.len();
        rules.push(RuleFile {
            source: self.id().to_string(),
            source_display_name: self.display_name().to_string(),
            scope: "project".to_string(),
            category: project_path.to_string_lossy().to_string(),
            filename: filename.to_string(),
            path: path.to_string_lossy().to_string(),
            content,
            size_bytes,
            enabled: true,
            toggleable: false,
            frontmatter: None,
        });
        Ok(())
    }

    fn discover_project_rule_roots(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for root in self.instruction_project_roots() {
            if root.exists()
                && (root.join("CLAUDE.md").exists() || root.join("AGENTS.md").exists())
                && seen.insert(root.to_string_lossy().to_string())
            {
                roots.push(root);
            }
        }
        roots
    }

    fn index_sources(&self) -> Vec<(PathBuf, Option<String>)> {
        let mut sources = Vec::new();
        if self.paths.projects_dir.exists() {
            sources.push((self.paths.projects_dir.clone(), None));
        }
        if self.paths.archive_root.exists() {
            if let Ok(archives) = fs::read_dir(&self.paths.archive_root) {
                for entry in archives.flatten() {
                    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let archive_name = entry.file_name().to_string_lossy().to_string();
                    let archive_projects = entry.path().join("projects");
                    if archive_projects.exists() {
                        sources.push((archive_projects, Some(archive_name)));
                    }
                }
            }
        }
        sources
    }

    fn index_entries(&self) -> Vec<(PathBuf, String, Option<String>)> {
        self.index_sources()
            .into_iter()
            .flat_map(|(projects_dir, archive_name)| {
                fs::read_dir(projects_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .flat_map(move |project_entry| {
                        let slug = project_entry.file_name().to_string_lossy().to_string();
                        let archive = archive_name.clone();
                        fs::read_dir(project_entry.path())
                            .into_iter()
                            .flatten()
                            .flatten()
                            .filter(|entry| {
                                entry.path().extension().is_some_and(|ext| ext == "jsonl")
                                    && entry.path().is_file()
                            })
                            .map(move |entry| (entry.path(), slug.clone(), archive.clone()))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn index_doc_for_session(
        &self,
        path: &Path,
        slug: &str,
        archive_name: Option<String>,
    ) -> Option<IndexDoc> {
        let meta = fs::metadata(path).ok()?;
        let session_id = path.file_stem()?.to_string_lossy().to_string();
        let modified = meta.modified().ok().and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH).ok().and_then(|d| {
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0).map(
                    |dt: chrono::DateTime<chrono::Utc>| {
                        let local: chrono::DateTime<chrono::Local> = dt.into();
                        local.format("%Y-%m-%d %H:%M").to_string()
                    },
                )
            })
        });

        let (first_prompt, agent_title, created_at, updated_at, texts, model_contexts, tokens) =
            extract_index_texts(path);
        let updated_at = updated_at.or(modified);
        let subagent_dir = path.parent()?.join(&session_id).join("subagents");
        let subagent_count = if subagent_dir.exists() {
            fs::read_dir(&subagent_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count() as u32
        } else {
            0
        };

        Some(IndexDoc {
            source: self.id().to_string(),
            session_id,
            project: slug.to_string(),
            project_path: Self::read_session_cwd(path, &self.host)
                .map(|cwd| cwd.to_string_lossy().to_string())
                .unwrap_or_else(|| self.host.decode_project_slug(slug)),
            created_at,
            timestamp: updated_at.clone(),
            updated_at,
            agent_title,
            file_size_bytes: meta.len(),
            subagent_count,
            archive_name,
            first_prompt,
            model_contexts,
            texts,
            tokens,
            key: path.to_string_lossy().to_string(),
            version: file_version(&meta),
        })
    }

    fn collect_rules(
        &self,
        dir: &Path,
        enabled: bool,
        rules: &mut Vec<RuleFile>,
    ) -> Result<(), AppError> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dirname = entry.file_name().to_string_lossy().to_string();
                let is_disabled = dirname == "_disabled";
                self.collect_rules(&path, if is_disabled { false } else { enabled }, rules)?;
            } else if path.extension().is_some_and(|e| e == "md") {
                let filename = entry.file_name().to_string_lossy().to_string();
                let rel = path
                    .parent()
                    .and_then(|p| p.strip_prefix(&self.paths.rules_dir).ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let category = rel
                    .replace("_disabled\\", "")
                    .replace("_disabled/", "")
                    .replace("_disabled", "");
                let category = if category.is_empty() {
                    "root".to_string()
                } else {
                    category
                };
                let content = fs::read_to_string(&path)?;
                let size_bytes = entry.metadata()?.len();
                let (yaml_str, _) = frontmatter::split_frontmatter(&content);
                let fm: Option<RuleFrontmatter> =
                    yaml_str.as_ref().and_then(|y| serde_yaml::from_str(y).ok());

                rules.push(RuleFile {
                    source: self.id().to_string(),
                    source_display_name: self.display_name().to_string(),
                    scope: "global".to_string(),
                    category,
                    filename,
                    path: path.to_string_lossy().to_string(),
                    content,
                    size_bytes,
                    enabled,
                    toggleable: true,
                    frontmatter: fm,
                });
            }
        }
        Ok(())
    }

    fn read_memory_file(&self, project: &str, path: &Path) -> Result<MemoryFile, AppError> {
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let content = fs::read_to_string(path)?;
        let size_bytes = fs::metadata(path)?.len();
        let (yaml_str, body) = frontmatter::split_frontmatter(&content);
        let mut fm: Option<MemoryFrontmatter> =
            yaml_str.as_ref().and_then(|y| serde_yaml::from_str(y).ok());
        if let Some(ref mut f) = fm {
            if f.memory_type.is_none() {
                f.memory_type = f.metadata.as_ref().and_then(|m| m.meta_type.clone());
            }
        }
        if filename == "MEMORY.md" {
            if let Some(ref mut f) = fm {
                f.memory_type = Some("index".to_string());
            } else {
                fm = Some(MemoryFrontmatter {
                    name: Some("MEMORY.md".to_string()),
                    description: Some("记忆索引文件".to_string()),
                    memory_type: Some("index".to_string()),
                    metadata: None,
                });
            }
        }

        Ok(MemoryFile {
            source: self.id().to_string(),
            source_display_name: self.display_name().to_string(),
            project: project.to_string(),
            project_path: self.host.decode_project_slug(project),
            filename,
            frontmatter: fm,
            content: body,
            size_bytes,
        })
    }
}

impl AgentProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn available(&self) -> bool {
        // A snapshot-only install is still readable: archived sessions and profiles live under
        // `.claude/_archives` after `projects` has deliberately been moved out of the live store.
        // Checking only `projects` made the UI disable Claude and hide those intact archives.
        self.paths.claude_dir.exists() || self.paths.archive_root.exists()
    }
    fn data_roots(&self) -> Vec<PathBuf> {
        vec![
            self.paths.projects_dir.clone(),
            self.paths.archive_root.clone(),
        ]
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sessions_read: true,
            sessions_search: true,
            sessions_resume: true,
            sessions_subagents: true,
            rules_read: true,
            rules_write: true,
            memory_read: true,
            memory_write: true,
            instructions_read: true,
            instructions_write: true,
            archive_read: true,
            archive_write: true,
            config_format: "json",
        }
    }

    fn global_instruction_candidates(&self) -> Vec<InstructionCandidate> {
        vec![
            InstructionCandidate {
                title: "全局 CLAUDE.md".to_string(),
                scope: "global",
                kind: "instructions",
                path: self.paths.claude_md.clone(),
                editable: true,
                include_missing: true,
                exists: None,
                size_bytes: None,
                description: "Claude Code 全局指令文件。".to_string(),
            },
            InstructionCandidate {
                title: "全局 settings.json".to_string(),
                scope: "global",
                kind: "config",
                path: self.paths.settings_json.clone(),
                editable: true,
                include_missing: true,
                exists: None,
                size_bytes: None,
                description: "Claude Code 全局设置文件。".to_string(),
            },
        ]
    }

    fn project_instruction_candidates(&self, project_path: &Path) -> Vec<InstructionCandidate> {
        vec![InstructionCandidate {
            title: "项目 CLAUDE.md".to_string(),
            scope: "project",
            kind: "instructions",
            path: project_path.join("CLAUDE.md"),
            editable: true,
            include_missing: false,
            exists: None,
            size_bytes: None,
            description: "项目目录中的 Claude Code 指令文件。".to_string(),
        }]
    }

    fn instruction_project_roots(&self) -> Vec<PathBuf> {
        const TTL: Duration = Duration::from_secs(30);
        if let Ok(mut cache) = self.project_roots_cache.lock() {
            if let Some((created, roots)) = cache.as_ref() {
                if created.elapsed() < TTL {
                    return roots.clone();
                }
            }
            // Coalesce concurrent rules/instructions loads into one filesystem pass.
            let roots = self.scan_instruction_project_roots();
            *cache = Some((Instant::now(), roots.clone()));
            return roots;
        }
        self.scan_instruction_project_roots()
    }

    fn resume_command(&self, session_id: &str, extra_args: &[String]) -> Option<String> {
        Some(
            format!(
                "claude --resume {} {}",
                quote_command_arg(session_id),
                extra_args.join(" ")
            )
            .trim()
            .to_string(),
        )
    }

    fn list_rules(&self) -> Result<Vec<RuleFile>, AppError> {
        let mut rules = Vec::new();
        self.collect_rules(&self.paths.rules_dir, true, &mut rules)?;
        if self.paths.claude_md.exists() {
            let content = fs::read_to_string(&self.paths.claude_md)?;
            let size_bytes = fs::metadata(&self.paths.claude_md)?.len();
            rules.push(RuleFile {
                source: self.id().to_string(),
                source_display_name: self.display_name().to_string(),
                scope: "global".to_string(),
                category: "global".to_string(),
                filename: "CLAUDE.md".to_string(),
                path: self.paths.claude_md.to_string_lossy().to_string(),
                content,
                size_bytes,
                enabled: true,
                toggleable: false,
                frontmatter: None,
            });
        }
        for project_path in self.discover_project_rule_roots() {
            self.add_project_rule_file(&mut rules, &project_path, "CLAUDE.md")?;
            self.add_project_rule_file(&mut rules, &project_path, "AGENTS.md")?;
        }
        rules.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then(a.filename.cmp(&b.filename))
        });
        Ok(rules)
    }

    fn get_rule(&self, category: &str, filename: &str) -> Result<RuleFile, AppError> {
        let path = self.paths.rules_dir.join(category).join(filename);
        let disabled_path = self
            .paths
            .rules_dir
            .join(category)
            .join("_disabled")
            .join(filename);

        let (actual_path, enabled) = if path.exists() {
            (path, true)
        } else if disabled_path.exists() {
            (disabled_path, false)
        } else {
            return Err(AppError::NotFound(format!("{}/{}", category, filename)));
        };

        let content = fs::read_to_string(&actual_path)?;
        let size_bytes = fs::metadata(&actual_path)?.len();
        let (yaml_str, _) = frontmatter::split_frontmatter(&content);
        let fm: Option<RuleFrontmatter> =
            yaml_str.as_ref().and_then(|y| serde_yaml::from_str(y).ok());

        Ok(RuleFile {
            source: self.id().to_string(),
            source_display_name: self.display_name().to_string(),
            scope: "global".to_string(),
            category: category.to_string(),
            filename: filename.to_string(),
            path: actual_path.to_string_lossy().to_string(),
            content,
            size_bytes,
            enabled,
            toggleable: true,
            frontmatter: fm,
        })
    }

    fn toggle_rule(&self, category: &str, filename: &str, enabled: bool) -> Result<(), AppError> {
        let enabled_path = self.paths.rules_dir.join(category).join(filename);
        let disabled_dir = self.paths.rules_dir.join(category).join("_disabled");
        let disabled_path = disabled_dir.join(filename);

        if enabled {
            if disabled_path.exists() {
                fs::rename(&disabled_path, &enabled_path)?;
            }
        } else if enabled_path.exists() {
            fs::create_dir_all(&disabled_dir)?;
            fs::rename(&enabled_path, &disabled_path)?;
        }
        Ok(())
    }

    fn list_workflows(&self) -> Vec<WorkflowItem> {
        let mut items = Vec::new();
        let make = |kind: &str, name: String, path: &Path| -> WorkflowItem {
            let content = fs::read_to_string(path).unwrap_or_default();
            let size_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            WorkflowItem {
                source: self.id().to_string(),
                source_display_name: self.display_name().to_string(),
                kind: kind.to_string(),
                name,
                scope: "global".to_string(),
                path: path.to_string_lossy().to_string(),
                description: workflow_description(&content),
                size_bytes,
            }
        };

        // Skills: one directory per skill, each containing a SKILL.md.
        if let Ok(entries) = fs::read_dir(&self.paths.skills_dir) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let skill_md = dir.join("SKILL.md");
                if skill_md.exists() {
                    let name = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    items.push(make("skill", name, &skill_md));
                }
            }
        }

        // Commands / plans / tasks: flat *.md files.
        let commands_dir = self.paths.claude_dir.join("commands");
        let flat: [(&str, &Path); 3] = [
            ("command", commands_dir.as_path()),
            ("plan", self.paths.plans_dir.as_path()),
            ("task", self.paths.tasks_dir.as_path()),
        ];
        for (kind, dir) in flat {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "md") {
                    let name = path
                        .file_stem()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    items.push(make(kind, name, &path));
                }
            }
        }

        items.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        items
    }

    fn read_workflow(&self, path: &str) -> Result<String, AppError> {
        // Only allow reads inside this provider's own workflow roots — never arbitrary files.
        let p = PathBuf::from(path);
        let commands_dir = self.paths.claude_dir.join("commands");
        let roots = [
            self.paths.skills_dir.as_path(),
            self.paths.plans_dir.as_path(),
            self.paths.tasks_dir.as_path(),
            commands_dir.as_path(),
        ];
        if !roots.iter().any(|root| p.starts_with(root)) || !p.exists() {
            return Err(AppError::NotFound(format!("workflow not found: {}", path)));
        }
        Ok(fs::read_to_string(&p)?)
    }

    fn list_memory_projects(&self) -> Result<Vec<ProjectInfo>, AppError> {
        claude_scanner::list_projects(&self.paths, &self.host)
    }

    fn list_memories(&self, project: &str) -> Result<Vec<MemoryFile>, AppError> {
        let mem_dir = self.paths.projects_dir.join(project).join("memory");
        if !mem_dir.exists() {
            return Ok(Vec::new());
        }
        let mut memories = Vec::new();
        for entry in fs::read_dir(&mem_dir)?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                memories.push(self.read_memory_file(project, &path)?);
            }
        }
        memories.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(memories)
    }

    fn get_memory(&self, project: &str, filename: &str) -> Result<MemoryFile, AppError> {
        let path = self
            .paths
            .projects_dir
            .join(project)
            .join("memory")
            .join(filename);
        if !path.exists() {
            return Err(AppError::NotFound(format!("{}/{}", project, filename)));
        }
        self.read_memory_file(project, &path)
    }

    fn save_memory(
        &self,
        project: &str,
        filename: &str,
        frontmatter_data: &MemoryFrontmatter,
        content: &str,
    ) -> Result<(), AppError> {
        let mem_dir = self.paths.projects_dir.join(project).join("memory");
        fs::create_dir_all(&mem_dir)?;
        let path = mem_dir.join(filename);
        let yaml = serde_yaml::to_string(frontmatter_data)?;
        let file_content = frontmatter::join_frontmatter(&yaml, content);
        fs::write(&path, file_content)?;
        Ok(())
    }

    fn delete_memory(&self, project: &str, filename: &str) -> Result<(), AppError> {
        let path = self
            .paths
            .projects_dir
            .join(project)
            .join("memory")
            .join(filename);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let index_path = self
            .paths
            .projects_dir
            .join(project)
            .join("memory")
            .join("MEMORY.md");
        if index_path.exists() {
            let content = fs::read_to_string(&index_path)?;
            let stem = filename.trim_end_matches(".md");
            let filtered: Vec<&str> = content
                .lines()
                .filter(|line| !line.contains(&format!("({}", filename)) && !line.contains(stem))
                .collect();
            fs::write(&index_path, filtered.join("\n") + "\n")?;
        }
        Ok(())
    }

    fn list_profiles(&self) -> Result<Vec<ProfileArchive>, AppError> {
        claude_archiver::list_profiles(&self.paths)
    }

    fn create_profile(&self, name: Option<String>) -> Result<ProfileArchive, AppError> {
        claude_archiver::create_profile(&self.paths, name)
    }

    fn restore_profile(&self, name: &str) -> Result<(), AppError> {
        claude_archiver::restore_profile(&self.paths, name)
    }

    fn delete_profile(&self, name: &str) -> Result<(), AppError> {
        claude_archiver::delete_profile(&self.paths, name)
    }

    fn rename_profile(&self, old_name: &str, new_name: &str) -> Result<(), AppError> {
        claude_archiver::rename_profile(&self.paths, old_name, new_name)
    }

    fn list_sessions(&self, project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
        if !self.paths.projects_dir.exists() {
            return Ok(Vec::new());
        }

        let project_dirs: Vec<PathBuf> = if let Some(p) = project {
            vec![self.paths.projects_dir.join(p)]
        } else {
            fs::read_dir(&self.paths.projects_dir)?
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect()
        };

        let mut session_entries = Vec::new();
        for proj_path in project_dirs {
            let slug = proj_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(dir_entries) = fs::read_dir(&proj_path) {
                for entry in dir_entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
                        session_entries.push((path, slug.clone()));
                    }
                }
            }
        }

        // Provider-level callers still receive complete native metadata. The sessions page uses
        // the summary index instead, so this full-fidelity compatibility path is not run alongside
        // the background indexer during normal navigation.
        let mut sessions: Vec<SessionSummary> = metadata_pool().install(|| {
            session_entries
                .into_par_iter()
                .filter_map(|(path, slug)| {
                    let display_path = Self::read_session_cwd(&path, &self.host)
                        .map(|cwd| cwd.to_string_lossy().to_string())
                        .unwrap_or_else(|| self.host.decode_project_slug(&slug));
                    jsonl::read_claude_session_summary_fast(&path, &slug, &display_path)
                })
                .collect()
        });
        sessions.sort_by(|a, b| {
            b.timestamp
                .as_deref()
                .unwrap_or("")
                .cmp(a.timestamp.as_deref().unwrap_or(""))
        });
        Ok(sessions)
    }

    fn index_documents(&self) -> IndexBatch {
        let results: Vec<Option<IndexDoc>> = self
            .index_entries()
            .par_iter()
            .map(|(path, slug, archive)| self.index_doc_for_session(path, slug, archive.clone()))
            .collect();
        let mut docs = Vec::with_capacity(results.len());
        let mut failed = 0;
        for r in results {
            match r {
                Some(d) => docs.push(d),
                None => failed += 1,
            }
        }
        IndexBatch { docs, failed }
    }

    fn index_manifest(&self) -> Vec<IndexManifestEntry> {
        self.index_entries()
            .into_iter()
            .filter_map(|(path, _slug, _archive)| {
                let meta = fs::metadata(&path).ok()?;
                Some(IndexManifestEntry {
                    key: path.to_string_lossy().to_string(),
                    version: file_version(&meta),
                })
            })
            .collect()
    }

    fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
        let results: Vec<Option<IndexDoc>> = self
            .index_entries()
            .par_iter()
            .filter(|(path, _, _)| only.contains(&path.to_string_lossy().to_string()))
            .map(|(path, slug, archive)| self.index_doc_for_session(path, slug, archive.clone()))
            .collect();
        let mut docs = Vec::with_capacity(results.len());
        let mut failed = 0;
        for r in results {
            match r {
                Some(d) => docs.push(d),
                None => failed += 1,
            }
        }
        IndexBatch { docs, failed }
    }

    fn first_prompt(
        &self,
        project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Option<String> {
        jsonl::read_claude_session_first_prompt(&self.session_file(project, session_id, archive))
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
        let path = self.session_file(project, session_id, archive);
        if !path.exists() {
            return Err(AppError::NotFound(format!(
                "Session not found: {}/{}",
                project, session_id
            )));
        }
        jsonl::read_claude_records_seekable(&path, byte_offset, limit, min_level)
    }

    fn session_tail(
        &self,
        project: &str,
        session_id: &str,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self.session_file(project, session_id, archive);
        if !path.exists() {
            return Err(AppError::NotFound(format!(
                "Session not found: {}/{}",
                project, session_id
            )));
        }
        jsonl::read_claude_records_tail(&path, limit, min_level)
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
        let path = self.session_file(project, session_id, archive);
        if !path.exists() {
            return Err(AppError::NotFound(format!(
                "Session not found: {}/{}",
                project, session_id
            )));
        }
        jsonl::read_claude_records_before(&path, before_offset, limit, min_level)
    }

    fn list_subagents(
        &self,
        project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SubagentInfo>, AppError> {
        let session_dir = self.base(archive).join(project).join(session_id);
        jsonl::list_claude_subagents(&session_dir)
    }

    fn subagent_detail(
        &self,
        project: &str,
        session_id: &str,
        agent_id: &str,
        byte_offset: u64,
        limit: u32,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let path = self
            .base(archive)
            .join(project)
            .join(session_id)
            .join("subagents")
            .join(format!("{}.jsonl", agent_id));
        if !path.exists() {
            return Err(AppError::NotFound(format!(
                "Subagent not found: {}",
                agent_id
            )));
        }
        // A subagent is a tool-execution trace — show its tool calls AND results ("tool" level).
        jsonl::read_claude_records_seekable(&path, byte_offset, limit, "tool")
    }

    fn search_in_session(
        &self,
        project: &str,
        session_id: &str,
        query: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let path = self.session_file(project, session_id, archive);
        if !path.exists() {
            return Err(AppError::NotFound("Session not found".into()));
        }
        Ok(jsonl::search_claude_session(&path, query))
    }
}

/// Extract Claude searchable text: user/assistant messages (content) + tool results (tool).
fn extract_index_texts(
    path: &Path,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<IndexText>,
    Vec<SessionModelInfo>,
    TokenUsage,
) {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => {
            return (
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                TokenUsage::default(),
            )
        }
    };
    let reader = BufReader::with_capacity(128 * 1024, file);
    let mut texts = FastIndexTextCollector::default();
    let mut first_prompt = None;
    let mut generated_title = None;
    let mut custom_title = None;
    let mut created_at = None;
    let mut updated_at = None;
    let mut model_contexts = Vec::new();
    let mut tokens = TokenUsage::default();

    for line in reader.lines().map_while(Result::ok) {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let rtype = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let role = val
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if rtype == "summary" {
            generated_title = val
                .get("summary")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
                .or(generated_title);
        } else if rtype == "custom-title" {
            custom_title = val
                .get("customTitle")
                .or_else(|| val.get("title"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
                .or(custom_title);
        }
        if rtype == "user" || rtype == "assistant" || role == "user" || role == "assistant" {
            if let Some(ts) = val.get("timestamp").and_then(|v| v.as_str()) {
                let ts = fmt_claude_ts(ts);
                if created_at.is_none() {
                    created_at = Some(ts.clone());
                }
                updated_at = Some(ts);
            }
        }

        if rtype == "assistant" || role == "assistant" {
            push_model_context(
                &mut model_contexts,
                None,
                val.get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                    .map(String::from),
                None,
            );
            // Sum token usage from each assistant turn's `message.usage` (standard Claude shape).
            if let Some(u) = val.get("message").and_then(|m| m.get("usage")) {
                let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let input = g("input_tokens");
                let output = g("output_tokens");
                let cache = g("cache_read_input_tokens") + g("cache_creation_input_tokens");
                tokens.input_tokens += input;
                tokens.output_tokens += output;
                tokens.cache_tokens += cache;
                tokens.total_tokens += input + output + cache;
            }
        }

        if rtype == "user" || rtype == "assistant" {
            if let Some(content) = val.get("message").and_then(|m| m.get("content")) {
                let text = if let Some(s) = content.as_str() {
                    if s.starts_with('<') {
                        String::new()
                    } else {
                        s.to_string()
                    }
                } else if let Some(arr) = content.as_array() {
                    arr.iter()
                        .filter_map(|item| {
                            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                item.get("text").and_then(|t| t.as_str()).map(String::from)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    if first_prompt.is_none()
                        && rtype == "user"
                        && val.get("isCompactSummary").and_then(|v| v.as_bool()) != Some(true)
                    {
                        first_prompt = Some(text.chars().take(200).collect());
                    }
                    texts.push("content", text);
                }
            }
        }

        // Extended-thinking blocks → "reasoning" scope, so the 思考 search filter actually works
        // for Claude (previously only content + tool were indexed, leaving 思考 always empty).
        if rtype == "assistant" {
            if let Some(arr) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                let thinking = arr
                    .iter()
                    .filter_map(|item| {
                        if item.get("type").and_then(|t| t.as_str()) == Some("thinking") {
                            item.get("thinking")
                                .and_then(|t| t.as_str())
                                .map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if !thinking.trim().is_empty() {
                    texts.push("reasoning", thinking);
                }
            }
        }

        if let Some(tool_result) = val.get("toolUseResult") {
            let text = if let Some(s) = tool_result.as_str() {
                s.to_string()
            } else {
                ["content", "result", "stdout", "output", "text"]
                    .iter()
                    .find_map(|key| {
                        tool_result
                            .get(key)
                            .and_then(|value| value.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_default()
            };
            if !text.trim().is_empty() {
                texts.push("tool", text);
            }
        }
    }

    (
        first_prompt,
        custom_title.or(generated_title),
        created_at,
        updated_at,
        texts.into_texts(),
        model_contexts,
        tokens,
    )
}

fn fmt_claude_ts(ts: &str) -> String {
    if let Ok(utc) = ts.parse::<chrono::DateTime<chrono::Utc>>() {
        let local: chrono::DateTime<chrono::Local> = utc.into();
        local.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        ts.replace('T', " ").chars().take(19).collect()
    }
}
