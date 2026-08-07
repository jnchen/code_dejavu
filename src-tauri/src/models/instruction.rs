use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct InstructionArtifact {
    pub source: String,
    pub source_display_name: String,
    pub title: String,
    pub scope: String,
    pub kind: String,
    pub path: String,
    pub exists: bool,
    pub editable: bool,
    pub size_bytes: u64,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstructionDetail {
    #[serde(flatten)]
    pub artifact: InstructionArtifact,
    pub content: String,
}
