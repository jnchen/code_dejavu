//! One agent, several machines.
//!
//! Claude Code / Codex / OpenCode / PiAgent can each be installed twice on the same box: natively,
//! and again inside one or more WSL distributions, each with its own `$HOME`. Those are not different tools,
//! so they must not become different sources in the UI — but they *are* different stores, with
//! independent session files, snapshots and rules.
//!
//! [`MultiHostProvider`] resolves that: it presents itself as one logical source and fans every
//! call out to one real provider per host, tagging the opaque keys
//! that come back so a later call can be routed to the store it came from. Each inner provider is
//! the ordinary, unmodified provider — it simply has a different home and [`Host`].
//!
//! Two kinds of identifier cross this boundary, and they are routed differently:
//!
//! * **Paths** (`project_path`, rule categories for project rules, instruction and workflow paths)
//!   are already readable and already carry the host — a WSL path is a `\\wsl.localhost\…` UNC
//!   path — so they are passed through untouched and routed with [`Host::of_path`]. Keeping them
//!   verbatim is what lets project instructions, "reveal in folder" and path comparison elsewhere
//!   in the app keep working without knowing hosts exist.
//! * **Opaque keys** (project slugs, snapshot names, global rule categories) have no host in them,
//!   so they get an `@wsl:<distro>/` prefix on the way out and are split again on the way in. A
//!   distro used by more than one account adds the account (`@wsl:<distro>~<user>/`), since a path
//!   alone cannot say which of them a key belongs to.
//!
//! WSL hosts are adopted *after* startup ([`MultiHostProvider::adopt`]): touching a distro's share
//! boots it, which is not something app launch should do synchronously.

use super::{
    AgentProvider, Capabilities, IndexBatch, IndexDoc, IndexManifestEntry, InstructionCandidate,
    WorkflowItem,
};
use crate::error::AppError;
use crate::hosts::{split_key, Host, HostHome};
use crate::models::memory::{MemoryFile, MemoryFrontmatter, ProjectInfo};
use crate::models::profile::ProfileArchive;
use crate::models::rule::RuleFile;
use crate::models::session::{PaginatedRecords, SessionSearchHit, SessionSummary, SubagentInfo};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Builds the agent's provider for a home directory belonging to `host`.
pub type HostFactory = Box<dyn Fn(Host, &Path) -> Arc<dyn AgentProvider> + Send + Sync>;

struct HostProvider {
    host: Host,
    provider: Arc<dyn AgentProvider>,
}

pub struct MultiHostProvider {
    native: Arc<dyn AgentProvider>,
    factory: HostFactory,
    extra: RwLock<Vec<HostProvider>>,
}

impl MultiHostProvider {
    pub fn new(native: Arc<dyn AgentProvider>, factory: HostFactory) -> Self {
        Self {
            native,
            factory,
            extra: RwLock::new(Vec::new()),
        }
    }

    /// Register discovered non-native homes. Idempotent, so a rescan cannot duplicate a host.
    pub fn adopt(&self, homes: &[HostHome]) {
        let Ok(mut extra) = self.extra.write() else {
            return;
        };
        for home in homes {
            if home.host.is_native() || extra.iter().any(|entry| entry.host == home.host) {
                continue;
            }
            extra.push(HostProvider {
                host: home.host.clone(),
                provider: (self.factory)(home.host.clone(), &home.home),
            });
        }
    }

    /// Every host backing this source, native first so it stays the default for untagged keys.
    fn hosts(&self) -> Vec<(Host, Arc<dyn AgentProvider>)> {
        let mut hosts = vec![(Host::Native, self.native.clone())];
        if let Ok(extra) = self.extra.read() {
            hosts.extend(
                extra
                    .iter()
                    .map(|entry| (entry.host.clone(), entry.provider.clone())),
            );
        }
        hosts
    }

    fn provider_for(&self, host: &Host) -> Arc<dyn AgentProvider> {
        if host.is_native() {
            return self.native.clone();
        }
        self.extra
            .read()
            .ok()
            .and_then(|extra| {
                extra
                    .iter()
                    .find(|entry| &entry.host == host)
                    // A host recovered from a path names the distro but not the account inside it.
                    // Any store in that distro reads the same filesystem, so the first one is the
                    // right answer for the path-routed calls that produce such a host.
                    .or_else(|| {
                        host.distro().and_then(|distro| {
                            extra
                                .iter()
                                .find(|entry| entry.host.distro() == Some(distro))
                        })
                    })
                    .map(|entry| entry.provider.clone())
            })
            .unwrap_or_else(|| self.native.clone())
    }

    /// Split a tagged key into the store it belongs to and the key that store knows it by.
    fn route<'a>(&self, key: &'a str) -> (Host, Arc<dyn AgentProvider>, &'a str) {
        let (host, inner) = split_key(key);
        (host.clone(), self.provider_for(&host), inner)
    }

    /// Route by a readable path: a WSL path names its own distro, so nothing needs tagging.
    fn route_path(&self, path: &Path) -> Arc<dyn AgentProvider> {
        self.provider_for(&Host::of_path(path))
    }

    /// Rule categories are a path for project rules and an opaque name for global ones, so both
    /// routings apply — tagged first, then the path form.
    fn route_category<'a>(&self, category: &'a str) -> (Arc<dyn AgentProvider>, &'a str) {
        let (host, inner) = split_key(category);
        if !host.is_native() {
            return (self.provider_for(&host), inner);
        }
        (self.route_path(Path::new(category)), category)
    }

    /// An optional archive name arriving alongside an already-routed key: it belongs to the same
    /// host, so only the tag has to come off.
    fn untag_archive(archive: Option<&str>) -> Option<&str> {
        archive.map(|name| split_key(name).1)
    }
}

/// Run `f` for every host and concatenate the results.
fn across<T>(
    hosts: Vec<(Host, Arc<dyn AgentProvider>)>,
    mut f: impl FnMut(&Host, &Arc<dyn AgentProvider>) -> Vec<T>,
) -> Vec<T> {
    hosts
        .iter()
        .flat_map(|(host, provider)| f(host, provider))
        .collect()
}

impl AgentProvider for MultiHostProvider {
    fn id(&self) -> &'static str {
        self.native.id()
    }

    fn display_name(&self) -> &'static str {
        self.native.display_name()
    }

    fn capabilities(&self) -> Capabilities {
        // Same agent everywhere; only the store differs.
        self.native.capabilities()
    }

    fn available(&self) -> bool {
        self.hosts()
            .iter()
            .any(|(_, provider)| provider.available())
    }

    fn hosts(&self) -> Vec<String> {
        MultiHostProvider::hosts(self)
            .into_iter()
            .filter_map(|(host, _)| host.tag())
            .collect()
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        across(self.hosts(), |_, provider| provider.data_roots())
    }

    fn list_sessions(&self, project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
        if let Some(project) = project {
            let (host, provider, inner) = self.route(project);
            let mut sessions = provider.list_sessions(Some(inner))?;
            for session in &mut sessions {
                tag_session(&host, session);
            }
            return Ok(sessions);
        }

        let mut all = Vec::new();
        for (host, provider) in self.hosts() {
            let mut sessions = provider.list_sessions(None)?;
            for session in &mut sessions {
                tag_session(&host, session);
            }
            all.append(&mut sessions);
        }
        Ok(all)
    }

    fn index_documents(&self) -> IndexBatch {
        let mut merged = IndexBatch::default();
        for (host, provider) in self.hosts() {
            let mut batch = provider.index_documents();
            for doc in &mut batch.docs {
                tag_doc(&host, doc);
            }
            merged.docs.append(&mut batch.docs);
            merged.failed += batch.failed;
        }
        merged
    }

    fn index_manifest(&self) -> Vec<IndexManifestEntry> {
        across(self.hosts(), |host, provider| {
            provider
                .index_manifest()
                .into_iter()
                .map(|entry| IndexManifestEntry {
                    key: host.tag_key(&entry.key),
                    version: entry.version,
                })
                .collect()
        })
    }

    fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
        let mut merged = IndexBatch::default();
        for (host, provider) in self.hosts() {
            // Each store only understands its own untagged keys, and must not be handed another
            // host's — an empty selection would otherwise be read as "reparse nothing".
            let wanted: HashSet<String> = only
                .iter()
                .filter_map(|key| {
                    let (key_host, inner) = split_key(key);
                    (key_host == host).then(|| inner.to_string())
                })
                .collect();
            if wanted.is_empty() {
                continue;
            }
            let mut batch = provider.index_documents_for(&wanted);
            for doc in &mut batch.docs {
                tag_doc(&host, doc);
            }
            merged.docs.append(&mut batch.docs);
            merged.failed += batch.failed;
        }
        merged
    }

    fn list_workflows(&self) -> Vec<WorkflowItem> {
        across(self.hosts(), |host, provider| {
            provider
                .list_workflows()
                .into_iter()
                .map(|mut item| {
                    if let Some(tag) = host.tag() {
                        item.name = format!("{} · {}", item.name, tag);
                    }
                    item
                })
                .collect()
        })
    }

    fn read_workflow(&self, path: &str) -> Result<String, AppError> {
        self.route_path(Path::new(path)).read_workflow(path)
    }

    fn global_instruction_candidates(&self) -> Vec<InstructionCandidate> {
        across(self.hosts(), |host, provider| {
            provider
                .global_instruction_candidates()
                .into_iter()
                .map(|candidate| tag_candidate(host, candidate))
                .collect()
        })
    }

    fn project_instruction_candidates(&self, project_path: &Path) -> Vec<InstructionCandidate> {
        self.route_path(project_path)
            .project_instruction_candidates(project_path)
    }

    fn instruction_project_roots(&self) -> Vec<PathBuf> {
        across(self.hosts(), |_, provider| {
            provider.instruction_project_roots()
        })
    }

    fn read_instruction_candidate(
        &self,
        candidate: &InstructionCandidate,
    ) -> Result<String, AppError> {
        self.route_path(&candidate.path)
            .read_instruction_candidate(candidate)
    }

    fn save_instruction_candidate(
        &self,
        candidate: &InstructionCandidate,
        content: &str,
    ) -> Result<(), AppError> {
        self.route_path(&candidate.path)
            .save_instruction_candidate(candidate, content)
    }

    fn resume_command(&self, session_id: &str, extra_args: &[String]) -> Option<String> {
        // The command is the agent's own CLI invocation and is identical on every host; which
        // machine it runs on is decided when the terminal is launched, from the project path.
        self.native.resume_command(session_id, extra_args)
    }

    fn list_rules(&self) -> Result<Vec<RuleFile>, AppError> {
        let mut all = Vec::new();
        for (host, provider) in self.hosts() {
            for mut rule in provider.list_rules()? {
                // Project rules are keyed by their project path, which already names the host and
                // is compared against project paths elsewhere — tagging it would break that match.
                if rule.scope != "project" {
                    rule.category = host.tag_key(&rule.category);
                }
                all.push(rule);
            }
        }
        Ok(all)
    }

    fn get_rule(&self, category: &str, filename: &str) -> Result<RuleFile, AppError> {
        let (provider, inner) = self.route_category(category);
        let mut rule = provider.get_rule(inner, filename)?;
        if rule.scope != "project" {
            rule.category = category.to_string();
        }
        Ok(rule)
    }

    fn toggle_rule(&self, category: &str, filename: &str, enabled: bool) -> Result<(), AppError> {
        let (provider, inner) = self.route_category(category);
        provider.toggle_rule(inner, filename, enabled)
    }

    fn list_memory_projects(&self) -> Result<Vec<ProjectInfo>, AppError> {
        let mut all = Vec::new();
        for (host, provider) in self.hosts() {
            for mut project in provider.list_memory_projects()? {
                project.slug = host.tag_key(&project.slug);
                all.push(project);
            }
        }
        Ok(all)
    }

    fn list_memories(&self, project: &str) -> Result<Vec<MemoryFile>, AppError> {
        let (host, provider, inner) = self.route(project);
        let mut memories = provider.list_memories(inner)?;
        for memory in &mut memories {
            memory.project = host.tag_key(&memory.project);
        }
        Ok(memories)
    }

    fn get_memory(&self, project: &str, filename: &str) -> Result<MemoryFile, AppError> {
        let (host, provider, inner) = self.route(project);
        let mut memory = provider.get_memory(inner, filename)?;
        memory.project = host.tag_key(&memory.project);
        Ok(memory)
    }

    fn save_memory(
        &self,
        project: &str,
        filename: &str,
        frontmatter_data: &MemoryFrontmatter,
        content: &str,
    ) -> Result<(), AppError> {
        let (_, provider, inner) = self.route(project);
        provider.save_memory(inner, filename, frontmatter_data, content)
    }

    fn delete_memory(&self, project: &str, filename: &str) -> Result<(), AppError> {
        let (_, provider, inner) = self.route(project);
        provider.delete_memory(inner, filename)
    }

    fn list_profiles(&self) -> Result<Vec<ProfileArchive>, AppError> {
        let mut all = Vec::new();
        for (host, provider) in self.hosts() {
            for mut profile in provider.list_profiles()? {
                profile.name = host.tag_key(&profile.name);
                if let Some(tag) = host.tag() {
                    profile.source_display_name =
                        format!("{} · {}", profile.source_display_name, tag);
                }
                all.push(profile);
            }
        }
        Ok(all)
    }

    fn create_profile(&self, name: Option<String>) -> Result<ProfileArchive, AppError> {
        // The snapshot label doubles as the host selector: an untagged label snapshots the native
        // install, which is what a single-host machine always means.
        let (host, provider, label) = match &name {
            Some(name) => {
                let (host, provider, inner) = self.route(name);
                (host, provider, Some(inner.to_string()))
            }
            None => (Host::Native, self.native.clone(), None),
        };
        let mut profile = provider.create_profile(label.filter(|l| !l.is_empty()))?;
        profile.name = host.tag_key(&profile.name);
        Ok(profile)
    }

    fn restore_profile(&self, name: &str) -> Result<(), AppError> {
        let (_, provider, inner) = self.route(name);
        provider.restore_profile(inner)
    }

    fn delete_profile(&self, name: &str) -> Result<(), AppError> {
        let (_, provider, inner) = self.route(name);
        provider.delete_profile(inner)
    }

    fn rename_profile(&self, old_name: &str, new_name: &str) -> Result<(), AppError> {
        let (_, provider, old_inner) = self.route(old_name);
        let (_, _, new_inner) = self.route(new_name);
        provider.rename_profile(old_inner, new_inner)
    }

    fn first_prompt(
        &self,
        project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Option<String> {
        let (_, provider, inner) = self.route(project);
        provider.first_prompt(inner, session_id, Self::untag_archive(archive))
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
        let (_, provider, inner) = self.route(project);
        provider.session_detail(
            inner,
            session_id,
            byte_offset,
            limit,
            min_level,
            Self::untag_archive(archive),
        )
    }

    fn session_tail(
        &self,
        project: &str,
        session_id: &str,
        limit: u32,
        min_level: &str,
        archive: Option<&str>,
    ) -> Result<PaginatedRecords, AppError> {
        let (_, provider, inner) = self.route(project);
        provider.session_tail(
            inner,
            session_id,
            limit,
            min_level,
            Self::untag_archive(archive),
        )
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
        let (_, provider, inner) = self.route(project);
        provider.session_before(
            inner,
            session_id,
            before_offset,
            limit,
            min_level,
            Self::untag_archive(archive),
        )
    }

    fn list_subagents(
        &self,
        project: &str,
        session_id: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SubagentInfo>, AppError> {
        let (_, provider, inner) = self.route(project);
        provider.list_subagents(inner, session_id, Self::untag_archive(archive))
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
        let (_, provider, inner) = self.route(project);
        provider.subagent_detail(
            inner,
            session_id,
            agent_id,
            byte_offset,
            limit,
            Self::untag_archive(archive),
        )
    }

    fn search_in_session(
        &self,
        project: &str,
        session_id: &str,
        query: &str,
        archive: Option<&str>,
    ) -> Result<Vec<SessionSearchHit>, AppError> {
        let (_, provider, inner) = self.route(project);
        provider.search_in_session(inner, session_id, query, Self::untag_archive(archive))
    }
}

fn tag_session(host: &Host, session: &mut SessionSummary) {
    session.project = host.tag_key(&session.project);
    session.archive_name = session
        .archive_name
        .as_ref()
        .map(|archive| host.tag_key(archive));
}

fn tag_doc(host: &Host, doc: &mut IndexDoc) {
    doc.project = host.tag_key(&doc.project);
    doc.key = host.tag_key(&doc.key);
    doc.archive_name = doc
        .archive_name
        .as_ref()
        .map(|archive| host.tag_key(archive));
}

fn tag_candidate(host: &Host, mut candidate: InstructionCandidate) -> InstructionCandidate {
    if let Some(tag) = host.tag() {
        candidate.title = format!("{} · {}", candidate.title, tag);
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A provider that answers with its own name and records the keys it was handed, so a test can
    /// assert both "which store was asked" and "what key did it see".
    struct FakeProvider {
        name: &'static str,
        calls: Mutex<Vec<String>>,
    }

    impl FakeProvider {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, call: String) {
            self.calls.lock().expect("calls").push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    fn summary(project: &str, archive: Option<&str>) -> SessionSummary {
        SessionSummary {
            source: "fake".to_string(),
            session_id: "s1".to_string(),
            project: project.to_string(),
            project_path: format!("/path/{}", project),
            first_prompt: None,
            agent_title: None,
            created_at: None,
            updated_at: None,
            timestamp: None,
            file_size_bytes: 0,
            subagent_count: 0,
            archive_name: archive.map(str::to_string),
            model_contexts: Vec::new(),
        }
    }

    fn rule(scope: &str, category: &str) -> RuleFile {
        RuleFile {
            source: "fake".to_string(),
            source_display_name: "Fake".to_string(),
            scope: scope.to_string(),
            category: category.to_string(),
            filename: "R.md".to_string(),
            path: format!("{}/R.md", category),
            content: String::new(),
            size_bytes: 0,
            enabled: true,
            toggleable: false,
            frontmatter: None,
        }
    }

    fn profile(name: &str) -> ProfileArchive {
        ProfileArchive {
            source: "fake".to_string(),
            source_display_name: "Fake".to_string(),
            name: name.to_string(),
            created: String::new(),
            items: 0,
            total_size: 0,
            size_human: "0 B".to_string(),
            note: None,
            is_auto: false,
        }
    }

    impl AgentProvider for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn display_name(&self) -> &'static str {
            "Fake"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                sessions_read: true,
                sessions_search: true,
                sessions_resume: true,
                sessions_subagents: false,
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

        fn available(&self) -> bool {
            true
        }

        fn list_sessions(&self, project: Option<&str>) -> Result<Vec<SessionSummary>, AppError> {
            self.record(format!("list_sessions:{}", project.unwrap_or("*")));
            Ok(vec![summary(self.name, Some("snap-1"))])
        }

        fn session_detail(
            &self,
            project: &str,
            _session_id: &str,
            _byte_offset: u64,
            _limit: u32,
            _min_level: &str,
            archive: Option<&str>,
        ) -> Result<PaginatedRecords, AppError> {
            self.record(format!("detail:{}:{}", project, archive.unwrap_or("-")));
            Err(AppError::NotFound(self.name.to_string()))
        }

        fn list_rules(&self) -> Result<Vec<RuleFile>, AppError> {
            Ok(vec![
                rule("global", "team"),
                rule("project", r"\\wsl.localhost\Ubuntu\home\me\app"),
            ])
        }

        fn get_rule(&self, category: &str, _filename: &str) -> Result<RuleFile, AppError> {
            self.record(format!("get_rule:{}", category));
            Ok(rule(
                if category.starts_with(r"\\") {
                    "project"
                } else {
                    "global"
                },
                category,
            ))
        }

        fn list_profiles(&self) -> Result<Vec<ProfileArchive>, AppError> {
            Ok(vec![profile("20260808-120000")])
        }

        fn restore_profile(&self, name: &str) -> Result<(), AppError> {
            self.record(format!("restore:{}", name));
            Ok(())
        }

        fn index_manifest(&self) -> Vec<IndexManifestEntry> {
            vec![IndexManifestEntry {
                key: format!("{}-doc", self.name),
                version: "1".to_string(),
            }]
        }

        fn index_documents_for(&self, only: &HashSet<String>) -> IndexBatch {
            let mut keys: Vec<String> = only.iter().cloned().collect();
            keys.sort();
            self.record(format!("index_for:{}", keys.join(",")));
            IndexBatch::default()
        }
    }

    fn ubuntu() -> Host {
        Host::Wsl {
            distro: "Ubuntu".to_string(),
            user: None,
        }
    }

    fn composite() -> (Arc<MultiHostProvider>, Arc<FakeProvider>, Arc<FakeProvider>) {
        let native = FakeProvider::new("native");
        let wsl = FakeProvider::new("wsl");
        let wsl_for_factory = wsl.clone();
        let multi = Arc::new(MultiHostProvider::new(
            native.clone(),
            Box::new(move |_, _| wsl_for_factory.clone()),
        ));
        multi.adopt(&[HostHome {
            host: ubuntu(),
            home: PathBuf::from(r"\\wsl.localhost\Ubuntu\home\me"),
        }]);
        (multi, native, wsl)
    }

    #[test]
    fn listing_merges_hosts_and_tags_only_the_non_native_keys() {
        let (multi, _, _) = composite();

        let sessions = multi.list_sessions(None).expect("sessions");

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.project.as_str())
                .collect::<Vec<_>>(),
            vec!["native", "@wsl:Ubuntu/wsl"]
        );
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.archive_name.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["snap-1", "@wsl:Ubuntu/snap-1"]
        );
    }

    #[test]
    fn a_tagged_key_reaches_its_own_store_untagged() {
        let (multi, native, wsl) = composite();

        multi
            .list_sessions(Some("@wsl:Ubuntu/proj"))
            .expect("sessions");
        let _ = multi.session_detail(
            "@wsl:Ubuntu/proj",
            "s1",
            0,
            10,
            "content",
            Some("@wsl:Ubuntu/snap-1"),
        );
        multi
            .restore_profile("@wsl:Ubuntu/20260808")
            .expect("restore");

        assert_eq!(
            wsl.calls(),
            vec![
                "list_sessions:proj",
                "detail:proj:snap-1",
                "restore:20260808"
            ]
        );
        assert!(native.calls().is_empty());
    }

    #[test]
    fn an_untagged_key_stays_on_the_native_store() {
        let (multi, native, wsl) = composite();

        multi.list_sessions(Some("C--Codes-app")).expect("sessions");
        multi.restore_profile("20260808").expect("restore");

        assert_eq!(
            native.calls(),
            vec!["list_sessions:C--Codes-app", "restore:20260808"]
        );
        assert!(wsl.calls().is_empty());
    }

    #[test]
    fn project_rule_categories_keep_their_path_so_project_lookups_still_match() {
        let (multi, _, wsl) = composite();

        let rules = multi.list_rules().expect("rules");
        let categories: Vec<&str> = rules.iter().map(|rule| rule.category.as_str()).collect();

        // Global categories are opaque names and need the host; project categories are paths that
        // already carry it and are compared against project paths elsewhere.
        assert_eq!(
            categories,
            vec![
                "team",
                r"\\wsl.localhost\Ubuntu\home\me\app",
                "@wsl:Ubuntu/team",
                r"\\wsl.localhost\Ubuntu\home\me\app",
            ]
        );

        multi
            .get_rule(r"\\wsl.localhost\Ubuntu\home\me\app", "R.md")
            .expect("project rule");
        multi
            .get_rule("@wsl:Ubuntu/team", "R.md")
            .expect("global rule");

        assert_eq!(
            wsl.calls(),
            vec![
                r"get_rule:\\wsl.localhost\Ubuntu\home\me\app",
                "get_rule:team"
            ]
        );
    }

    #[test]
    fn incremental_reindex_hands_each_store_only_its_own_keys() {
        let (multi, native, wsl) = composite();

        let manifest: Vec<String> = multi
            .index_manifest()
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        assert_eq!(manifest, vec!["native-doc", "@wsl:Ubuntu/wsl-doc"]);

        multi.index_documents_for(&manifest.into_iter().collect());

        assert_eq!(native.calls(), vec!["index_for:native-doc"]);
        assert_eq!(wsl.calls(), vec!["index_for:wsl-doc"]);
    }

    #[test]
    fn a_store_with_nothing_to_reparse_is_not_asked_to_reparse_everything() {
        let (multi, native, wsl) = composite();

        multi.index_documents_for(&HashSet::from(["@wsl:Ubuntu/wsl-doc".to_string()]));

        // An empty key set means "nothing changed here", which the default provider impl would
        // read as a full reparse — so the native store must not be called at all.
        assert!(native.calls().is_empty());
        assert_eq!(wsl.calls(), vec!["index_for:wsl-doc"]);
    }
}
