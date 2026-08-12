use crate::agents::ProviderRegistry;
use crate::error::AppError;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};
use tauri::State;

/// One USD-per-1M-tokens pricing row, matched by case-insensitive substring on the model id.
/// Used by the Usage page to estimate cost. Editable in Settings, persisted in the app config.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct PriceRow {
    #[serde(rename = "match")]
    pub matcher: String,
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct DejavuConfig {
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub agent_args: HashMap<String, Vec<String>>,
    #[serde(default = "default_prices")]
    pub prices: Vec<PriceRow>,
    /// Whether to look for agent installs inside WSL distributions. On by default: a machine with
    /// no WSL pays one cheap `wsl.exe --list` at startup, and a machine that runs its agents inside
    /// WSL would otherwise show nothing at all.
    #[serde(default = "default_wsl_scan")]
    pub wsl_scan: bool,
    /// Distributions to leave alone. Reading a distro's share starts it, so this is the escape
    /// hatch for one that is slow, huge, or simply not interesting.
    #[serde(default)]
    pub wsl_excluded: Vec<String>,
    #[serde(default, rename = "claude_args", skip_serializing)]
    pub legacy_claude_args: Vec<String>,
}

impl DejavuConfig {
    fn normalize(mut self) -> Self {
        if !self.legacy_claude_args.is_empty() {
            let claude_args = std::mem::take(&mut self.legacy_claude_args);
            self.agent_args
                .entry("claude".to_string())
                .or_insert(claude_args);
        }
        self.prices.retain(|row| {
            !matches!(
                (
                    row.matcher.to_ascii_lowercase().as_str(),
                    row.input,
                    row.output
                ),
                ("o1", 15.0, 60.0) | ("o3", 15.0, 60.0)
            )
        });
        // Older releases shipped bare `opus`/`sonnet`/`haiku` substring rows. Keep custom edits
        // intact, but migrate untouched built-ins to real provider/model prefixes.
        for row in &mut self.prices {
            let replacement = match (
                row.matcher.to_ascii_lowercase().as_str(),
                row.input,
                row.output,
            ) {
                ("opus", 15.0, 75.0) => Some("claude-opus"),
                ("sonnet", 3.0, 15.0) => Some("claude-sonnet"),
                ("haiku", 0.8, 4.0) => Some("claude-haiku"),
                _ => None,
            };
            if let Some(replacement) = replacement {
                row.matcher = replacement.to_string();
            }
        }
        self
    }

    fn args_for(&self, source: &str) -> Vec<String> {
        self.agent_args.get(source).cloned().unwrap_or_default()
    }
}

fn default_prices() -> Vec<PriceRow> {
    [
        ("claude-opus", 15.0, 75.0),
        ("claude-sonnet", 3.0, 15.0),
        ("claude-haiku", 0.8, 4.0),
        ("claude-fable-5", 10.0, 50.0),
        ("claude-haiku-4-5", 1.0, 5.0),
        ("claude-opus-4-6", 5.0, 25.0),
        ("claude-opus-4-7", 5.0, 25.0),
        ("claude-opus-4-8", 5.0, 25.0),
        ("claude-sonnet-4-6", 3.0, 15.0),
        ("gpt-5", 1.25, 10.0),
        ("gpt-5.4", 2.5, 15.0),
        ("gpt-5.4-mini", 0.75, 4.5),
        ("gpt-5.4-pro", 30.0, 180.0),
        ("gpt-5.5", 5.0, 30.0),
        ("gpt-5.6-sol", 5.0, 30.0),
        ("gpt-4o", 2.5, 10.0),
        ("gpt-4", 2.5, 10.0),
        ("gemini", 1.25, 5.0),
    ]
    .into_iter()
    .map(|(matcher, input, output)| PriceRow {
        matcher: matcher.to_string(),
        input,
        output,
    })
    .collect()
}

fn default_wsl_scan() -> bool {
    true
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        if which_exists("pwsh") {
            "pwsh".to_string()
        } else {
            "powershell".to_string()
        }
    }
    #[cfg(not(windows))]
    {
        "bash".to_string()
    }
}

#[cfg(windows)]
fn which_exists(name: &str) -> bool {
    // Do not shell out to `where`: in the packaged Windows GUI app that can briefly flash a
    // console window when config defaults are loaded (for example, opening the Usage page reads
    // the price config). A direct PATH/PATHEXT check is enough for choosing the default shell.
    let path = match std::env::var_os("PATH") {
        Some(path) => path,
        None => return false,
    };
    let has_ext = std::path::Path::new(name).extension().is_some();
    let pathexts: Vec<String> = if has_ext {
        vec![String::new()]
    } else {
        std::env::var_os("PATHEXT")
            .map(|v| {
                v.to_string_lossy()
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| ext.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_else(|| {
                vec![
                    ".com".to_string(),
                    ".exe".to_string(),
                    ".bat".to_string(),
                    ".cmd".to_string(),
                ]
            })
    };

    for dir in std::env::split_paths(&path) {
        if has_ext {
            if dir.join(name).is_file() {
                return true;
            }
            continue;
        }
        for ext in &pathexts {
            if dir.join(format!("{}{}", name, ext)).is_file() {
                return true;
            }
        }
    }
    false
}

impl Default for DejavuConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            env: HashMap::new(),
            agent_args: HashMap::new(),
            prices: default_prices(),
            wsl_scan: default_wsl_scan(),
            wsl_excluded: Vec::new(),
            legacy_claude_args: Vec::new(),
        }
    }
}

/// An agent process group associated with a session. The PID and start time are both returned so a
/// later stop request cannot accidentally target a recycled PID.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionProcessInfo {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub command: String,
    pub cwd: Option<String>,
    pub started_at: u64,
    pub run_time_seconds: u64,
    pub process_count: u32,
    pub match_reason: String,
}

fn process_command_full(process: &sysinfo::Process) -> String {
    let args: Vec<String> = process
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let command = if args.is_empty() {
        process.name().to_string_lossy().into_owned()
    } else {
        args.join(" ")
    };
    command
}

fn process_command(process: &sysinfo::Process) -> String {
    // Command lines can contain very large prompts or arbitrary arguments. Keep the process
    // picker responsive and avoid turning the UI into an accidental log of an entire command.
    let command = process_command_full(process);
    command.chars().take(600).collect()
}

fn is_pi_executable_name(value: &std::ffi::OsStr) -> bool {
    // Split both path separators explicitly so Windows command paths remain testable on macOS.
    let raw = value.to_string_lossy();
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&raw)
        .to_ascii_lowercase();
    matches!(name.as_str(), "pi" | "pi.exe" | "pi.cmd")
}

fn is_pi_package_path(value: &std::ffi::OsStr) -> bool {
    value
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('\\', "/")
        .contains("/pi-coding-agent/")
}

fn process_matches_source(process: &sysinfo::Process, source: &str) -> bool {
    if source == "pi" {
        if is_pi_executable_name(process.name())
            || process
                .exe()
                .is_some_and(|path| is_pi_executable_name(path.as_os_str()))
            || process
                .cmd()
                .first()
                .is_some_and(|arg| is_pi_executable_name(arg))
        {
            return true;
        }
        // npm installations often expose the process as `node .../pi-coding-agent/.../cli.js`.
        // Match the package directory, never a generic `contains("pi")`, because the CLI name is
        // too short and would otherwise classify unrelated applications as PiAgent.
        return process.cmd().iter().any(|arg| is_pi_package_path(arg));
    }
    let needle = match source {
        "codex" => "codex",
        "claude" => "claude",
        "opencode" => "opencode",
        _ => return false,
    };
    let name = process.name().to_string_lossy().to_ascii_lowercase();
    let exe = process
        .exe()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let first_arg = process
        .cmd()
        .first()
        .map(|arg| {
            Path::new(arg)
                .file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_else(|| arg.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_default();
    name.contains(needle) || exe.contains(needle) || first_arg.contains(needle)
}

fn normalized_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn process_cwd_matches(process: &sysinfo::Process, project_path: &Path) -> bool {
    let Some(cwd) = process.cwd() else {
        return false;
    };
    normalized_path(cwd) == normalized_path(project_path)
}

fn command_mentions_project(command: &str, project_path: &Path) -> bool {
    let command = command.to_ascii_lowercase().replace('\\', "/");
    let project = project_path
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('\\', "/");
    !project.is_empty() && command.contains(&project)
}

fn session_lock_exists(session_id: &str) -> bool {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return false;
    }
    home_dir()
        .join(".codex")
        .join("thread-writer-locks")
        .join(format!("{}.lock", session_id))
        .is_file()
}

fn process_matches_session(
    process: &sysinfo::Process,
    project_path: &Path,
    session_id: &str,
    source: &str,
) -> Option<&'static str> {
    if !process_matches_source(process, source) {
        return None;
    }
    let command = process_command_full(process);
    if !session_id.trim().is_empty() && command.contains(session_id) {
        return Some("session-id");
    }
    // A Codex process can keep the same project cwd while several historical sessions from that
    // project remain indexed. The per-session writer lock is the active-session signal; require it
    // before using cwd/command project matching so history cannot inherit the newest process.
    if source == "codex"
        && session_lock_exists(session_id)
        && !project_path.as_os_str().is_empty()
        && process_cwd_matches(process, project_path)
    {
        return Some("session-lock");
    }
    // Windows can deny process-cwd reads unless the app has the necessary query privilege. Some
    // app-server launchers include their project directory in the command line, which is a safe
    // fallback for that case (and also helps when a server changes its cwd after startup).
    if source != "codex"
        && !project_path.as_os_str().is_empty()
        && process_cwd_matches(process, project_path)
    {
        return Some("project");
    }
    if source == "codex"
        && session_lock_exists(session_id)
        && command_mentions_project(&command, project_path)
    {
        return Some("session-lock-command");
    }
    if source != "codex" && command_mentions_project(&command, project_path) {
        return Some("project-command");
    }
    None
}

fn process_parent_map(system: &System) -> HashMap<Pid, Option<Pid>> {
    system
        .processes()
        .iter()
        .map(|(pid, process)| (*pid, process.parent()))
        .collect()
}

fn refresh_processes_for_matching(system: &mut System) {
    // The short refresh API intentionally skips command lines and working directories. Those
    // fields are required to associate an agent process with the session, especially on macOS.
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet),
    );
}

fn agent_pids(system: &System, source: &str) -> HashSet<Pid> {
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| process_matches_source(process, source).then_some(*pid))
        .collect()
}

fn agent_root_pid(pid: Pid, parents: &HashMap<Pid, Option<Pid>>, agents: &HashSet<Pid>) -> Pid {
    let mut root = pid;
    for _ in 0..64 {
        let Some(Some(parent)) = parents.get(&root) else {
            break;
        };
        if !agents.contains(parent) {
            break;
        }
        root = *parent;
    }
    root
}

fn process_info_for(
    system: &System,
    root: Pid,
    parents: &HashMap<Pid, Option<Pid>>,
    agents: &HashSet<Pid>,
    match_reason: &str,
) -> Option<SessionProcessInfo> {
    let process = system.process(root)?;
    let process_count = agents
        .iter()
        .filter(|pid| agent_root_pid(**pid, parents, agents) == root)
        .count()
        .min(u32::MAX as usize) as u32;
    Some(SessionProcessInfo {
        pid: usize::from(root).min(u32::MAX as usize) as u32,
        parent_pid: process
            .parent()
            .map(|pid| usize::from(pid).min(u32::MAX as usize) as u32),
        command: process_command(process),
        cwd: process
            .cwd()
            .map(|path| path.to_string_lossy().into_owned()),
        started_at: process.start_time(),
        run_time_seconds: process.run_time(),
        process_count,
        match_reason: match_reason.to_string(),
    })
}

fn list_session_processes_blocking(
    project_path: String,
    session_id: String,
    source: Option<String>,
) -> Result<Vec<SessionProcessInfo>, AppError> {
    let source = source.as_deref().unwrap_or("codex");
    if !matches!(source, "codex" | "claude" | "opencode" | "pi") {
        return Ok(Vec::new());
    }
    let mut system = System::new();
    refresh_processes_for_matching(&mut system);
    let project_path = PathBuf::from(project_path);
    let parents = process_parent_map(&system);
    let agents = agent_pids(&system, source);
    let mut roots: HashMap<Pid, &'static str> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(reason) = process_matches_session(process, &project_path, &session_id, source) {
            let root = agent_root_pid(*pid, &parents, &agents);
            roots
                .entry(root)
                .and_modify(|existing| {
                    if *existing != "session-id" && reason == "session-id" {
                        *existing = reason;
                    }
                })
                .or_insert(reason);
        }
    }
    let mut result: Vec<_> = roots
        .into_iter()
        .filter_map(|(root, reason)| process_info_for(&system, root, &parents, &agents, reason))
        .collect();
    result.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.pid.cmp(&a.pid)));
    Ok(result)
}

fn stop_session_process_blocking(
    pid: u32,
    started_at: u64,
    project_path: String,
    session_id: String,
    source: Option<String>,
) -> Result<(), AppError> {
    if source.as_deref().is_some_and(|source| source != "codex") {
        return Err(AppError::Archive("只有 Codex 会话支持进程管理".to_string()));
    }
    let mut system = System::new();
    refresh_processes_for_matching(&mut system);
    let target = Pid::from(pid as usize);
    let process = system
        .process(target)
        .ok_or_else(|| AppError::NotFound(format!("进程 {} 已不存在", pid)))?;
    if process.start_time() != started_at {
        return Err(AppError::Archive(format!("进程 {} 已被其他进程复用", pid)));
    }
    let project_path = PathBuf::from(project_path);
    if process_matches_session(process, &project_path, &session_id, "codex").is_none() {
        return Err(AppError::Archive(
            "目标进程不再属于当前 Codex 会话".to_string(),
        ));
    }

    let parents = process_parent_map(&system);
    let agents = agent_pids(&system, "codex");
    let root = agent_root_pid(target, &parents, &agents);
    let group: Vec<Pid> = agents
        .iter()
        .copied()
        .filter(|candidate| agent_root_pid(*candidate, &parents, &agents) == root)
        .collect();
    let mut attempted = false;
    // Stop children first so a helper cannot immediately respawn while the app-server is exiting.
    for candidate in group.iter().rev() {
        if let Some(process) = system.process(*candidate) {
            attempted |= process.kill_with(Signal::Term).unwrap_or(false);
        }
    }
    if let Some(process) = system.process(root) {
        attempted |= process.kill_with(Signal::Term).unwrap_or(false);
    }
    if !attempted {
        return Err(AppError::Archive(format!("无法关闭进程 {}", pid)));
    }

    thread::sleep(Duration::from_millis(250));
    refresh_processes_for_matching(&mut system);
    // A Codex helper may ignore TERM. Escalate only within the already-validated process group.
    for candidate in group.into_iter().chain(std::iter::once(root)) {
        if let Some(process) = system.process(candidate) {
            let _ = process.kill();
        }
    }
    Ok(())
}

fn config_path() -> std::path::PathBuf {
    let new_path = app_config_dir().join("config.json");
    if new_path.exists() {
        new_path
    } else {
        legacy_config_path()
    }
}

fn save_config_path() -> std::path::PathBuf {
    app_config_dir().join("config.json")
}

fn legacy_config_path() -> std::path::PathBuf {
    home_dir().join(".claude").join("dejavu.json")
}

fn home_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
    }

    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
    }
}

fn app_config_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("USERPROFILE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join(".config")
            })
            .join("CodeDejavu")
    }

    #[cfg(not(windows))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join(".config")
            })
            .join("code-dejavu")
    }
}

pub fn load_config() -> DejavuConfig {
    let path = config_path();
    if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<DejavuConfig>(&s).ok())
            .unwrap_or_default()
            .normalize()
    } else {
        DejavuConfig::default()
    }
}

#[tauri::command]
pub async fn get_dejavu_config() -> Result<DejavuConfig, AppError> {
    tauri::async_runtime::spawn_blocking(|| Ok(load_config()))
        .await
        .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn save_dejavu_config(config: DejavuConfig) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let json = serde_json::to_string_pretty(&config.normalize())?;
        let path = save_config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, json)?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn resume_session(
    registry: State<'_, ProviderRegistry>,
    project_path: String,
    session_id: String,
    source: Option<String>,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let provider = registry
            .resolve(source.as_deref())
            .ok_or_else(|| AppError::NotFound(format!("Unknown agent source: {:?}", source)))?;
        if !provider.capabilities().sessions_resume {
            return Err(AppError::Archive(format!(
                "{} 不支持恢复会话",
                provider.display_name()
            )));
        }
        let config = load_config();
        let args = config.args_for(provider.id());
        let command = provider.resume_command(&session_id, &args).ok_or_else(|| {
            AppError::Archive(format!("{} 没有恢复命令", provider.display_name()))
        })?;
        launch_shell(&config, &project_path, Some(command))
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Find agent processes attached to the current session. Codex uses its per-session writer lock for
/// project-based matching; Claude, OpenCode and PiAgent expose project-level associations for
/// inspection.
/// A process is returned once per agent process tree.
#[tauri::command]
pub async fn list_session_processes(
    project_path: String,
    session_id: String,
    source: Option<String>,
) -> Result<Vec<SessionProcessInfo>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        list_session_processes_blocking(project_path, session_id, source)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Stop a previously listed Codex process tree after revalidating its PID, start time, command and
/// working directory. Claude, OpenCode and PiAgent are intentionally view-only.
#[tauri::command]
pub async fn stop_session_process(
    pid: u32,
    started_at: u64,
    project_path: String,
    session_id: String,
    source: Option<String>,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        stop_session_process_blocking(pid, started_at, project_path, session_id, source)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Open an http(s) URL in the system default browser (not inside the webview).
#[tauri::command]
pub async fn open_external(app: tauri::AppHandle, url: String) -> Result<(), AppError> {
    use tauri_plugin_opener::OpenerExt;
    // Only allow web links — never file:// or arbitrary protocols/commands.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(AppError::Archive(format!("不支持的链接: {}", url)));
    }
    // Use the opener plugin — robust on Windows (ShellExecute), handles `&`/query strings.
    tauri::async_runtime::spawn_blocking(move || {
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| AppError::Archive(format!("无法打开链接: {}", e)))
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

/// Write an exported transcript into the app's `exports/` dir and return the saved file path.
#[tauri::command]
pub async fn save_text_export(filename: String, content: String) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || save_text_export_blocking(filename, content))
        .await
        .map_err(|e| AppError::Archive(e.to_string()))?
}

fn save_text_export_blocking(filename: String, content: String) -> Result<String, AppError> {
    let safe: String = filename
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.trim().is_empty() {
        "export.md".to_string()
    } else {
        safe
    };
    let dir = crate::paths::app_data_dir().join("exports");
    fs::create_dir_all(&dir)?;
    let path = dir.join(safe);
    fs::write(&path, content)?;
    Ok(path.to_string_lossy().to_string())
}

/// Reveal a saved file by opening its containing folder in the system file manager.
#[tauri::command]
pub async fn reveal_path(app: tauri::AppHandle, path: String) -> Result<(), AppError> {
    use tauri_plugin_opener::OpenerExt;
    tauri::async_runtime::spawn_blocking(move || {
        let p = std::path::PathBuf::from(&path);
        let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or(p);
        app.opener()
            .open_path(dir.to_string_lossy().to_string(), None::<&str>)
            .map_err(|e| AppError::Archive(format!("无法打开文件夹: {}", e)))
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn open_in_terminal(project_path: String) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let config = load_config();
        launch_shell(&config, &project_path, None)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[cfg(test)]
mod process_match_tests {
    use super::{is_pi_executable_name, is_pi_package_path};
    use std::ffi::OsStr;

    #[test]
    fn pi_process_names_are_exact_and_cross_platform() {
        assert!(is_pi_executable_name(OsStr::new("/usr/local/bin/pi")));
        assert!(is_pi_executable_name(OsStr::new(
            r"C:\\Users\\me\\bin\\pi.cmd"
        )));
        assert!(is_pi_executable_name(OsStr::new("PI.EXE")));
        assert!(!is_pi_executable_name(OsStr::new("pilot")));
        assert!(!is_pi_executable_name(OsStr::new("pip")));
    }

    #[test]
    fn scoped_npm_package_path_identifies_pi_without_matching_generic_node() {
        assert!(is_pi_package_path(OsStr::new(
            "/usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
        )));
        assert!(is_pi_package_path(OsStr::new(
            r"C:\\npm\\node_modules\\@earendil-works\\pi-coding-agent\\dist\\cli.js"
        )));
        assert!(!is_pi_package_path(OsStr::new(
            "/usr/lib/node_modules/typescript/bin/tsc"
        )));
    }
}

/// POSIX single-quote: wrap in '...', escaping embedded single quotes as '\''.
/// Makes an arbitrary string safe as ONE shell word for bash/zsh/sh.
fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// PowerShell single-quote: wrap in '...', doubling embedded single quotes.
fn pwsh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// cmd.exe has no reliable in-quote escaping, so drop the characters that could
/// break out of `set "K=V"` / `cd /d "..."` or inject (`"` and `%`).
fn cmd_value(s: &str) -> String {
    s.replace(['"', '%'], "")
}

/// Keep only safe env-var name characters so a crafted key can't inject shell syntax.
fn sanitize_env_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Escape a string for embedding inside an AppleScript double-quoted literal.
#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The bash script run inside a distro for a project directory.
///
/// Separated from the spawn so it can be executed and asserted on directly — the interesting part
/// is the quoting, and a launched terminal window proves nothing to a test.
#[cfg(windows)]
pub(crate) fn wsl_shell_script(
    config: &DejavuConfig,
    cwd: &str,
    main_cmd: Option<String>,
    keep_open: bool,
) -> String {
    let mut parts = vec![format!("cd {}", posix_quote(cwd))];
    parts.extend(
        config
            .env
            .iter()
            .map(|(key, value)| format_env_set("bash", key, value))
            .filter(|part| !part.is_empty()),
    );
    if let Some(cmd) = main_cmd.filter(|cmd| !cmd.is_empty()) {
        parts.push(cmd);
    }
    let script = parts.join(" && ");
    if keep_open {
        // Keep the window open afterwards, the way the native branches do.
        format!("{}; exec ${{SHELL:-bash}}", script)
    } else {
        script
    }
}

/// Open a terminal *inside* the WSL distribution the project lives on.
///
/// The project path reaching us is the readable UNC form (`\\wsl.localhost\Ubuntu\home\me\app`);
/// the shell on the other side only understands `/home/me/app`, so it is translated back. The
/// configured shell is ignored here on purpose: whatever the user picked for Windows, the shell
/// that can actually run the agent lives in the distro.
///
/// The command is introduced with `-e`, **not** `--`. Everything after `--` is re-split by
/// `wsl.exe` on whitespace, which quietly shreds a quoted script: `cd 'a b' && export X=1` arrives
/// as a handful of separate words and bash runs fragments of it. `-e` passes the argument vector
/// through intact.
#[cfg(windows)]
fn launch_wsl_shell(
    config: &DejavuConfig,
    distro: &str,
    project_path: &str,
    main_cmd: Option<String>,
) -> Result<(), AppError> {
    let host = crate::hosts::Host::Wsl {
        distro: distro.to_string(),
        user: None,
    };
    let cwd = host.to_agent_path(std::path::Path::new(project_path));
    let script = wsl_shell_script(config, &cwd, main_cmd, true);

    // `start` reads a leading quoted argument as the window title, so give it an explicit empty
    // one rather than letting it swallow part of the command.
    Command::new("cmd")
        .args([
            "/c", "start", "", "wsl.exe", "-d", distro, "-e", "bash", "-lc",
        ])
        .arg(&script)
        .spawn()
        .map_err(|e| AppError::Archive(format!("无法打开 WSL 终端（{}）: {}", distro, e)))?;
    Ok(())
}

fn launch_shell(
    config: &DejavuConfig,
    project_path: &str,
    main_cmd: Option<String>,
) -> Result<(), AppError> {
    // A WSL project is identified by its own path, so nothing has to be threaded down here.
    #[cfg(windows)]
    if let crate::hosts::Host::Wsl { distro, .. } =
        crate::hosts::Host::of_path(std::path::Path::new(project_path))
    {
        return launch_wsl_shell(config, &distro, project_path, main_cmd);
    }

    let env_cmds: String = config
        .env
        .iter()
        .map(|(k, v)| format_env_set(&config.shell, k, v))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" && ");

    let main_cmd = main_cmd.unwrap_or_default();

    let full_cmd = if env_cmds.is_empty() {
        if main_cmd.is_empty() {
            String::new()
        } else {
            main_cmd
        }
    } else if main_cmd.is_empty() {
        env_cmds
    } else {
        format!("{} && {}", env_cmds, main_cmd)
    };

    #[cfg(windows)]
    {
        match config.shell.as_str() {
            "pwsh" | "powershell" => {
                let cd = format!("Set-Location {}", pwsh_quote(project_path));
                let script = if full_cmd.is_empty() {
                    cd
                } else {
                    format!("{}; {}", cd, full_cmd.replace("&&", ";"))
                };
                Command::new("cmd")
                    .args(["/c", "start", &config.shell, "-NoExit", "-Command", &script])
                    .spawn()
                    .map_err(|e| AppError::Archive(format!("无法打开终端: {}", e)))?;
            }
            "bash" | "git-bash" => {
                let cd = format!("cd {}", posix_quote(project_path));
                let script = if full_cmd.is_empty() {
                    cd
                } else {
                    format!("{} && {}", cd, full_cmd)
                };
                Command::new("cmd")
                    .args([
                        "/c",
                        "start",
                        "bash",
                        "-c",
                        &format!("{}; exec bash", script),
                    ])
                    .spawn()
                    .map_err(|e| AppError::Archive(format!("无法打开终端: {}", e)))?;
            }
            _ => {
                let body = if full_cmd.is_empty() {
                    String::new()
                } else {
                    format!(" && {}", full_cmd)
                };
                let script = format!("cd /d \"{}\"{}", cmd_value(project_path), body);
                Command::new("cmd")
                    .args(["/c", "start", "cmd", "/k", &script])
                    .spawn()
                    .map_err(|e| AppError::Archive(format!("无法打开终端: {}", e)))?;
            }
        }
    }

    // macOS: drive Terminal.app via osascript so env + resume actually run. Args are passed
    // directly to `osascript` (no shell), so only AppleScript-string escaping is needed; the
    // project path is POSIX-quoted inside the script it runs.
    #[cfg(target_os = "macos")]
    {
        let script = if full_cmd.is_empty() {
            format!("cd {}", posix_quote(project_path))
        } else {
            format!("cd {} && {}", posix_quote(project_path), full_cmd)
        };
        let applescript = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            applescript_escape(&script)
        );
        Command::new("osascript")
            .arg("-e")
            .arg(&applescript)
            .spawn()
            .map_err(|e| AppError::Archive(format!("无法打开终端: {}", e)))?;
    }

    // Other unix (Linux): try common terminal emulators. The script is passed as a single
    // `bash -c` argument (not through a shell), so the POSIX-quoted path is the only untrusted bit.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let script = if full_cmd.is_empty() {
            format!("cd {}; exec ${{SHELL:-bash}}", posix_quote(project_path))
        } else {
            format!(
                "cd {} && {}; exec ${{SHELL:-bash}}",
                posix_quote(project_path),
                full_cmd
            )
        };
        let attempts: [&[&str]; 4] = [
            &["x-terminal-emulator", "-e", "bash", "-c"],
            &["gnome-terminal", "--", "bash", "-c"],
            &["konsole", "-e", "bash", "-c"],
            &["xterm", "-e", "bash", "-c"],
        ];
        let launched = attempts.iter().any(|argv| {
            Command::new(argv[0])
                .args(&argv[1..])
                .arg(&script)
                .spawn()
                .is_ok()
        });
        if !launched {
            return Err(AppError::Archive(
                "未找到可用的终端模拟器（已尝试 x-terminal-emulator/gnome-terminal/konsole/xterm）"
                    .to_string(),
            ));
        }
    }

    Ok(())
}

fn format_env_set(shell: &str, key: &str, value: &str) -> String {
    let key = sanitize_env_key(key);
    if key.is_empty() {
        return String::new();
    }
    match shell {
        "pwsh" | "powershell" => format!("$env:{}={}", key, pwsh_quote(value)),
        "cmd" => format!("set \"{}={}\"", key, cmd_value(value)),
        // bash / zsh / sh / git-bash
        _ => format!("export {}={}", key, posix_quote(value)),
    }
}
