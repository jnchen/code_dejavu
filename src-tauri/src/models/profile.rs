use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileArchive {
    pub source: String,
    pub source_display_name: String,
    pub name: String,
    pub created: String,
    pub items: u32,
    pub total_size: u64,
    pub size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub is_auto: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveMeta {
    pub created: String,
    pub name: String,
    pub items: u32,
    #[serde(rename = "totalSize")]
    pub total_size: u64,
    #[serde(rename = "sizeHuman")]
    pub size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
