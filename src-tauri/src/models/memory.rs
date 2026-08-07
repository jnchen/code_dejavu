use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryFrontmatter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MemoryMetadata>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryMetadata {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub meta_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct MemoryFile {
    pub source: String,
    pub source_display_name: String,
    pub project: String,
    pub project_path: String,
    pub filename: String,
    pub frontmatter: Option<MemoryFrontmatter>,
    pub content: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct ProjectInfo {
    pub source: String,
    pub source_display_name: String,
    pub slug: String,
    pub display_path: String,
    pub memory_count: u32,
    pub session_count: u32,
    pub last_active: Option<String>,
}
