//! Read-only structured view of every agent's MCP servers and hooks.
//!
//! Secret values are never returned: only environment/header key names are exposed.

use crate::agents::ProviderRegistry;
use crate::error::AppError;
use crate::paths::ClaudePaths;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::State;

#[derive(Clone, Serialize)]
pub struct McpServer {
    pub name: String,
    pub scope: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub enabled: bool,
}

#[derive(Clone, Serialize)]
pub struct HookEntry {
    pub event: String,
    pub matcher: String,
    pub commands: Vec<String>,
}

#[derive(Serialize)]
pub struct AgentToolsInfo {
    pub source: String,
    pub source_display_name: String,
    pub available: bool,
    pub mcp_servers: Vec<McpServer>,
    pub hooks: Vec<HookEntry>,
    pub mcp_source_paths: Vec<String>,
    pub hooks_source_paths: Vec<String>,
}

#[derive(Serialize)]
pub struct ToolsInfo {
    pub sources: Vec<AgentToolsInfo>,
}

#[tauri::command]
pub async fn list_tools(
    paths: State<'_, ClaudePaths>,
    registry: State<'_, ProviderRegistry>,
) -> Result<ToolsInfo, AppError> {
    let claude_json = paths.claude_json.clone();
    let settings_json = paths.settings_json.clone();
    let sources = registry.sources();
    let providers = registry.providers();
    tauri::async_runtime::spawn_blocking(move || {
        let home = home();
        let mut result = Vec::new();
        for source in sources {
            let mut info = AgentToolsInfo {
                source: source.id.clone(),
                source_display_name: source.display_name,
                available: source.available,
                mcp_servers: Vec::new(),
                hooks: Vec::new(),
                mcp_source_paths: Vec::new(),
                hooks_source_paths: Vec::new(),
            };
            match source.id.as_str() {
                "claude" => collect_claude(&claude_json, &settings_json, &mut info),
                "codex" => {
                    let roots = providers
                        .iter()
                        .find(|provider| provider.id() == "codex")
                        .map(|provider| provider.instruction_project_roots())
                        .unwrap_or_default();
                    collect_codex(&home.join(".codex").join("config.toml"), &roots, &mut info);
                }
                "opencode" => {
                    let roots = providers
                        .iter()
                        .find(|provider| provider.id() == "opencode")
                        .map(|provider| provider.instruction_project_roots())
                        .unwrap_or_default();
                    collect_opencode(&home.join(".config").join("opencode"), &roots, &mut info);
                }
                _ => {}
            }
            sort_info(&mut info);
            result.push(info);
        }
        Ok(ToolsInfo { sources: result })
    })
    .await
    .map_err(|error| AppError::Archive(error.to_string()))?
}

fn home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn collect_claude(claude_json: &Path, settings_json: &Path, info: &mut AgentToolsInfo) {
    if let Some(root) = read_json(claude_json) {
        collect_json_mcp(root.get("mcpServers"), "global", &mut info.mcp_servers);
        if let Some(projects) = root.get("projects").and_then(Value::as_object) {
            for (project, value) in projects {
                collect_json_mcp(value.get("mcpServers"), project, &mut info.mcp_servers);
            }
        }
        info.mcp_source_paths.push(path_string(claude_json));
    }
    if let Some(settings) = read_json(settings_json) {
        if let Some(hooks) = settings.get("hooks").and_then(Value::as_object) {
            for (event, groups) in hooks {
                for group in groups.as_array().into_iter().flatten() {
                    let matcher = group
                        .get("matcher")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let commands = group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                        .map(String::from)
                        .collect();
                    info.hooks.push(HookEntry {
                        event: event.clone(),
                        matcher,
                        commands,
                    });
                }
            }
        }
        info.hooks_source_paths.push(path_string(settings_json));
    }
}

fn collect_codex(global_config: &Path, project_roots: &[PathBuf], info: &mut AgentToolsInfo) {
    collect_codex_file(global_config, "global", info);
    for root in project_roots {
        collect_codex_file(
            &root.join(".codex").join("config.toml"),
            &root.to_string_lossy(),
            info,
        );
    }
}

#[derive(Default)]
struct TomlMcp {
    command: String,
    url: String,
    args: Vec<String>,
    env_keys: Vec<String>,
    enabled: bool,
}

fn collect_codex_file(path: &Path, scope: &str, info: &mut AgentToolsInfo) {
    let Ok(body) = fs::read_to_string(path) else {
        return;
    };
    info.mcp_source_paths.push(path_string(path));
    let mut servers: HashMap<String, TomlMcp> = HashMap::new();
    let mut section = String::new();
    for raw_line in body.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(['"', '\'']);
        let value = raw_value.trim();
        if section.is_empty() && key == "notify" {
            let commands = parse_toml_array(value);
            if !commands.is_empty() {
                info.hooks.push(HookEntry {
                    event: "notify".to_string(),
                    matcher: String::new(),
                    commands,
                });
                info.hooks_source_paths.push(path_string(path));
            }
            continue;
        }
        let Some(rest) = section.strip_prefix("mcp_servers.") else {
            continue;
        };
        let mut parts = rest.split('.');
        let name = parts
            .next()
            .unwrap_or("")
            .trim_matches(['"', '\''])
            .to_string();
        if name.is_empty() {
            continue;
        }
        let nested = parts.next().unwrap_or("");
        let server = servers.entry(name).or_insert_with(|| TomlMcp {
            enabled: true,
            ..Default::default()
        });
        if nested == "env" || nested == "http_headers" || nested == "headers" {
            if !server.env_keys.iter().any(|existing| existing == key) {
                server.env_keys.push(key.to_string());
            }
        } else {
            match key {
                "command" => server.command = parse_toml_string(value),
                "url" => server.url = parse_toml_string(value),
                "args" => server.args = parse_toml_array(value),
                "enabled" => server.enabled = value != "false",
                "disabled" => server.enabled = value != "true",
                _ => {}
            }
        }
    }
    for (name, server) in servers {
        let (transport, command) = if !server.url.is_empty() {
            ("http".to_string(), server.url)
        } else {
            ("stdio".to_string(), server.command)
        };
        info.mcp_servers.push(McpServer {
            name,
            scope: scope.to_string(),
            transport,
            command,
            args: server.args,
            env_keys: server.env_keys,
            enabled: server.enabled,
        });
    }
}

fn collect_opencode(config_dir: &Path, project_roots: &[PathBuf], info: &mut AgentToolsInfo) {
    for filename in ["opencode.json", "opencode.jsonc"] {
        collect_opencode_file(&config_dir.join(filename), "global", info);
    }
    for root in project_roots {
        for filename in ["opencode.json", "opencode.jsonc"] {
            collect_opencode_file(&root.join(filename), &root.to_string_lossy(), info);
        }
    }
}

fn collect_opencode_file(path: &Path, scope: &str, info: &mut AgentToolsInfo) {
    let Ok(body) = fs::read_to_string(path) else {
        return;
    };
    let Some(root) = parse_jsonc(&body) else {
        return;
    };
    let Some(mcp) = root.get("mcp").and_then(Value::as_object) else {
        return;
    };
    info.mcp_source_paths.push(path_string(path));
    for (name, config) in mcp {
        let kind = config.get("type").and_then(Value::as_str).unwrap_or("");
        let url = config.get("url").and_then(Value::as_str).unwrap_or("");
        let command_parts: Vec<String> = config
            .get("command")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        let command = if !url.is_empty() {
            url.to_string()
        } else {
            command_parts.first().cloned().unwrap_or_default()
        };
        let args = if command_parts.len() > 1 {
            command_parts[1..].to_vec()
        } else {
            Vec::new()
        };
        let mut env_keys = Vec::new();
        for field in ["environment", "env", "headers"] {
            if let Some(map) = config.get(field).and_then(Value::as_object) {
                env_keys.extend(map.keys().cloned());
            }
        }
        env_keys.sort();
        env_keys.dedup();
        info.mcp_servers.push(McpServer {
            name: name.clone(),
            scope: scope.to_string(),
            transport: if kind == "remote" || !url.is_empty() {
                "http".to_string()
            } else {
                "stdio".to_string()
            },
            command,
            args,
            env_keys,
            enabled: config
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
}

fn collect_json_mcp(value: Option<&Value>, scope: &str, out: &mut Vec<McpServer>) {
    let Some(object) = value.and_then(Value::as_object) else {
        return;
    };
    for (name, config) in object {
        let command = config.get("command").and_then(Value::as_str);
        let url = config.get("url").and_then(Value::as_str);
        let transport = config
            .get("type")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| {
                if url.is_some() {
                    "http".to_string()
                } else if command.is_some() {
                    "stdio".to_string()
                } else {
                    "unknown".to_string()
                }
            });
        let args = config
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        let env_keys = config
            .get("env")
            .and_then(Value::as_object)
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        out.push(McpServer {
            name: name.clone(),
            scope: scope.to_string(),
            transport,
            command: command.or(url).unwrap_or("").to_string(),
            args,
            env_keys,
            enabled: config
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
}

fn read_json(path: &Path) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
}

fn parse_jsonc(body: &str) -> Option<Value> {
    serde_json::from_str(body)
        .ok()
        .or_else(|| serde_json::from_str(&strip_jsonc_comments(body)).ok())
}

fn strip_jsonc_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let chars: Vec<char> = body.chars().collect();
    let mut index = 0;
    let mut quoted = false;
    let mut escaped = false;
    while index < chars.len() {
        let ch = chars[index];
        if quoted {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            index += 1;
        } else if ch == '"' {
            quoted = true;
            out.push(ch);
            index += 1;
        } else if ch == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
        } else if ch == '/' && chars.get(index + 1) == Some(&'*') {
            index += 2;
            while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                index += 1;
            }
            index = (index + 2).min(chars.len());
        } else {
            out.push(ch);
            index += 1;
        }
    }
    out
}

fn strip_toml_comment(line: &str) -> &str {
    let mut quoted = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if let Some(quote) = quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' && quote == '"' {
                escaped = true;
            } else if ch == quote {
                quoted = None;
            }
        } else if ch == '"' || ch == '\'' {
            quoted = Some(ch);
        } else if ch == '#' {
            return &line[..index];
        }
    }
    line
}

fn parse_toml_string(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character| character == '"' || character == '\'')
        .replace("\\\"", "\"")
}

fn parse_toml_array(value: &str) -> Vec<String> {
    let value = value.trim().trim_start_matches('[').trim_end_matches(']');
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in value.chars() {
        if let Some(active) = quote {
            if escaped {
                current.push(ch);
                escaped = false;
            } else if ch == '\\' && active == '"' {
                escaped = true;
            } else if ch == active {
                quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == ',' {
            let item = current.trim();
            if !item.is_empty() {
                items.push(item.to_string());
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }
    let item = current.trim();
    if !item.is_empty() {
        items.push(item.to_string());
    }
    items
}

fn sort_info(info: &mut AgentToolsInfo) {
    info.mcp_servers.sort_by(|left, right| {
        left.scope
            .cmp(&right.scope)
            .then(left.name.cmp(&right.name))
    });
    info.hooks.sort_by(|left, right| {
        left.event
            .cmp(&right.event)
            .then(left.matcher.cmp(&right.matcher))
    });
    let mut seen = HashSet::new();
    info.mcp_source_paths
        .retain(|path| seen.insert(path.clone()));
    seen.clear();
    info.hooks_source_paths
        .retain(|path| seen.insert(path.clone()));
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_toml_helpers_keep_values_and_hide_comments() {
        assert_eq!(
            parse_toml_array(r#"["cmd", "/c", "npx"]"#),
            vec!["cmd", "/c", "npx"]
        );
        assert_eq!(
            strip_toml_comment(r#"url = "https://x/#ok" # no"#),
            r#"url = "https://x/#ok" "#
        );
    }

    #[test]
    fn jsonc_comment_stripping_preserves_urls() {
        let value = parse_jsonc(
            r#"{"url":"https://example.test/x",// comment
"enabled":true}"#,
        )
        .expect("parse jsonc");
        assert_eq!(value["url"], "https://example.test/x");
    }
}
