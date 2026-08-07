use crate::models::instruction::InstructionDetail;
use crate::models::memory::{MemoryFile, ProjectInfo};
use crate::models::rule::RuleFile;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ProjectContextStatus {
    pub supported: bool,
    pub writable: bool,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectContext {
    pub source: String,
    pub source_display_name: String,
    pub project: String,
    pub project_path: String,
    pub instructions: Vec<InstructionDetail>,
    pub configs: Vec<InstructionDetail>,
    pub rules: Vec<RuleFile>,
    pub memories: Vec<MemoryFile>,
    pub memory_project: Option<ProjectInfo>,
    pub instructions_status: ProjectContextStatus,
    pub rules_status: ProjectContextStatus,
    pub memory_status: ProjectContextStatus,
}
