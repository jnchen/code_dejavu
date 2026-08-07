use crate::agents::ProviderRegistry;
use crate::error::AppError;
use std::collections::HashMap;
use std::fs;
use std::process::Command;
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
        self
    }

    fn args_for(&self, source: &str) -> Vec<String> {
        self.agent_args.get(source).cloned().unwrap_or_default()
    }
}

fn default_prices() -> Vec<PriceRow> {
    [
        ("opus", 15.0, 75.0),
        ("sonnet", 3.0, 15.0),
        ("haiku", 0.8, 4.0),
        ("o3", 15.0, 60.0),
        ("o1", 15.0, 60.0),
        ("gpt-5", 1.25, 10.0),
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
            legacy_claude_args: Vec::new(),
        }
    }
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

fn load_config() -> DejavuConfig {
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

fn launch_shell(
    config: &DejavuConfig,
    project_path: &str,
    main_cmd: Option<String>,
) -> Result<(), AppError> {
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
