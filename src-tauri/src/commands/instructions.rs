use crate::agents::{AgentProvider, InstructionCandidate, ProviderRegistry, SourceInfo};
use crate::error::AppError;
use crate::models::instruction::{InstructionArtifact, InstructionDetail};
use crate::models::project_context::{ProjectContext, ProjectContextStatus};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub async fn list_instruction_artifacts(
    registry: State<'_, ProviderRegistry>,
) -> Result<Vec<InstructionArtifact>, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || Ok(discover_instruction_artifacts(&registry)))
        .await
        .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_instruction_artifact(
    registry: State<'_, ProviderRegistry>,
    source: String,
    path: String,
) -> Result<InstructionDetail, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let (provider, source_info, candidate) = find_candidate(&registry, &source, &path)?;
        let artifact = artifact_from_candidate(&source_info, &candidate);
        let content = provider.read_instruction_candidate(&candidate)?;
        Ok(InstructionDetail { artifact, content })
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn save_instruction_artifact(
    registry: State<'_, ProviderRegistry>,
    source: String,
    path: String,
    content: String,
) -> Result<(), AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        let (provider, source_info, candidate) = find_candidate(&registry, &source, &path)?;
        let artifact = artifact_from_candidate(&source_info, &candidate);
        if !artifact.editable {
            return Err(AppError::Archive(format!(
                "Instruction artifact is read-only: {}",
                artifact.path
            )));
        }
        provider.save_instruction_candidate(&candidate, &content)?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

#[tauri::command]
pub async fn get_project_context(
    registry: State<'_, ProviderRegistry>,
    source: String,
    project: String,
    project_path: String,
) -> Result<ProjectContext, AppError> {
    let registry = ProviderRegistry::new(registry.providers());
    tauri::async_runtime::spawn_blocking(move || {
        get_project_context_blocking(&registry, source, project, project_path)
    })
    .await
    .map_err(|e| AppError::Archive(e.to_string()))?
}

fn get_project_context_blocking(
    registry: &ProviderRegistry,
    source: String,
    project: String,
    project_path: String,
) -> Result<ProjectContext, AppError> {
    let source_info = source_info_by_id(registry, &source)?;
    let provider = registry
        .get(&source)
        .ok_or_else(|| AppError::NotFound(format!("instruction source: {}", source)))?;
    let capabilities = source_info.capabilities.clone();
    let project_root = std::path::PathBuf::from(&project_path);

    let details = if capabilities.instructions_read {
        project_instruction_details(&provider, &source_info, &project_root)?
    } else {
        Vec::new()
    };
    let artifact_paths: HashSet<String> = details
        .iter()
        .map(|detail| normalize_path(&detail.artifact.path))
        .collect();

    let mut instructions = Vec::new();
    let mut configs = Vec::new();
    for detail in details {
        if detail.artifact.kind == "config" {
            configs.push(detail);
        } else {
            instructions.push(detail);
        }
    }

    let rules = if capabilities.rules_read {
        provider
            .list_rules()?
            .into_iter()
            .filter(|rule| {
                rule.scope == "project"
                    && same_path(&rule.category, &project_path)
                    && !artifact_paths.contains(&normalize_path(&rule.path))
            })
            .collect()
    } else {
        Vec::new()
    };

    let (memory_project, memories, memory_status) = if capabilities.memory_read {
        let memory_projects = provider.list_memory_projects()?;
        let found_project = memory_projects.into_iter().find(|candidate| {
            candidate.slug == project || same_path(&candidate.display_path, &project_path)
        });
        if let Some(project_info) = found_project {
            let memories = provider.list_memories(&project_info.slug)?;
            let message = if memories.is_empty() {
                if capabilities.memory_write {
                    "当前 cwd 暂无项目记忆，可新建。".to_string()
                } else {
                    "当前 cwd 暂无项目记忆。".to_string()
                }
            } else {
                "只显示当前 cwd 绑定的项目记忆。".to_string()
            };
            (
                Some(project_info),
                memories,
                ProjectContextStatus {
                    supported: true,
                    writable: capabilities.memory_write,
                    message,
                },
            )
        } else {
            (
                None,
                Vec::new(),
                ProjectContextStatus {
                    supported: true,
                    writable: capabilities.memory_write,
                    message: "已发现记忆能力，但没有与当前 cwd 绑定的项目记忆。".to_string(),
                },
            )
        }
    } else {
        (
            None,
            Vec::new(),
            ProjectContextStatus {
                supported: false,
                writable: false,
                message: format!("{} 当前没有项目记忆接口。", source_info.display_name),
            },
        )
    };

    Ok(ProjectContext {
        source: source_info.id,
        source_display_name: source_info.display_name,
        project,
        project_path,
        instructions,
        configs,
        rules,
        memories,
        memory_project,
        instructions_status: ProjectContextStatus {
            supported: capabilities.instructions_read,
            writable: capabilities.instructions_write,
            message: if capabilities.instructions_read {
                "只列当前 cwd 下 Agent 会读取的项目指令文件；未创建的文件可直接创建。".to_string()
            } else {
                "当前 Agent 没有项目指令文件接口。".to_string()
            },
        },
        rules_status: ProjectContextStatus {
            supported: capabilities.rules_read,
            writable: capabilities.rules_write,
            message: if capabilities.rules_read {
                "只列当前 cwd 的独立项目规则；全局规则不在这里管理。".to_string()
            } else {
                "当前 Agent 没有独立规则接口。".to_string()
            },
        },
        memory_status,
    })
}

fn find_candidate(
    registry: &ProviderRegistry,
    source: &str,
    path: &str,
) -> Result<(Arc<dyn AgentProvider>, SourceInfo, InstructionCandidate), AppError> {
    let source_info = source_info_by_id(registry, source)?;
    if !source_info.capabilities.instructions_read {
        return Err(AppError::Archive(format!(
            "{} 没有可读取的指令文件",
            source_info.display_name
        )));
    }
    let provider = registry
        .get(source)
        .ok_or_else(|| AppError::NotFound(format!("instruction source: {}", source)))?;

    for candidate in provider
        .global_instruction_candidates()
        .into_iter()
        .chain(project_instruction_candidates(&provider, &source_info))
    {
        if candidate.path.to_string_lossy() == path {
            return Ok((provider, source_info, candidate));
        }
    }

    Err(AppError::NotFound(format!(
        "instruction artifact: {} {}",
        source, path
    )))
}

fn source_info_by_id(registry: &ProviderRegistry, source: &str) -> Result<SourceInfo, AppError> {
    registry
        .sources()
        .into_iter()
        .find(|candidate| candidate.id == source)
        .ok_or_else(|| AppError::NotFound(format!("instruction source: {}", source)))
}

fn discover_instruction_artifacts(registry: &ProviderRegistry) -> Vec<InstructionArtifact> {
    let providers: Vec<(SourceInfo, Arc<dyn AgentProvider>)> = registry
        .sources()
        .into_iter()
        .filter(|source| source.capabilities.instructions_read)
        .filter_map(|source| registry.get(&source.id).map(|provider| (source, provider)))
        .collect();
    let mut artifacts: Vec<InstructionArtifact> = providers
        .into_par_iter()
        .flat_map_iter(|(source, provider)| {
            let mut artifacts = Vec::new();
            let mut seen = HashSet::new();
            for candidate in provider.global_instruction_candidates() {
                push_candidate(&mut artifacts, &mut seen, &source, candidate);
            }
            for candidate in project_instruction_candidates(&provider, &source) {
                push_candidate(&mut artifacts, &mut seen, &source, candidate);
            }
            artifacts
        })
        .collect();

    artifacts.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then(a.scope.cmp(&b.scope))
            .then(a.title.cmp(&b.title))
            .then(a.path.cmp(&b.path))
    });
    artifacts
}

fn project_instruction_candidates(
    provider: &Arc<dyn AgentProvider>,
    source: &SourceInfo,
) -> Vec<InstructionCandidate> {
    if !source.available {
        return Vec::new();
    }
    provider
        .instruction_project_roots()
        .into_par_iter()
        .flat_map_iter(|project_path| provider.project_instruction_candidates(&project_path))
        .collect()
}

fn project_instruction_details(
    provider: &Arc<dyn AgentProvider>,
    source: &SourceInfo,
    project_path: &Path,
) -> Result<Vec<InstructionDetail>, AppError> {
    let mut details = Vec::new();
    let mut seen = HashSet::new();
    for candidate in provider.project_instruction_candidates(project_path) {
        let artifact = artifact_from_candidate(source, &candidate);
        if !artifact.exists && !artifact.editable && !candidate.include_missing {
            continue;
        }
        let key = normalize_path(&artifact.path);
        if !seen.insert(key) {
            continue;
        }
        let content = provider.read_instruction_candidate(&candidate)?;
        details.push(InstructionDetail { artifact, content });
    }
    details.sort_by(|a, b| {
        a.artifact
            .kind
            .cmp(&b.artifact.kind)
            .then(a.artifact.title.cmp(&b.artifact.title))
            .then(a.artifact.path.cmp(&b.artifact.path))
    });
    Ok(details)
}

fn push_candidate(
    artifacts: &mut Vec<InstructionArtifact>,
    seen: &mut HashSet<String>,
    source: &crate::agents::SourceInfo,
    candidate: InstructionCandidate,
) {
    let artifact = artifact_from_candidate(source, &candidate);
    let exists = artifact.exists;
    if !exists && !candidate.include_missing {
        return;
    }
    let key = format!("{}\0{}", source.id, artifact.path);
    if !seen.insert(key) {
        return;
    }
    artifacts.push(artifact);
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn same_path(a: &str, b: &str) -> bool {
    normalize_path(a) == normalize_path(b)
}

fn artifact_from_candidate(
    source: &crate::agents::SourceInfo,
    candidate: &InstructionCandidate,
) -> InstructionArtifact {
    let exists = candidate.exists.unwrap_or_else(|| candidate.path.exists());
    let path = candidate.path.to_string_lossy().to_string();
    InstructionArtifact {
        source: source.id.clone(),
        source_display_name: source.display_name.clone(),
        title: candidate.title.clone(),
        scope: candidate.scope.to_string(),
        kind: candidate.kind.to_string(),
        path,
        exists,
        editable: candidate.editable && source.capabilities.instructions_write,
        size_bytes: candidate.size_bytes.unwrap_or_else(|| {
            if exists {
                fs::metadata(&candidate.path)
                    .map(|meta| meta.len())
                    .unwrap_or(0)
            } else {
                0
            }
        }),
        description: candidate.description.clone(),
    }
}
