use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RuleFrontmatter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub globs: Option<String>,
    #[serde(rename = "alwaysApply", skip_serializing_if = "Option::is_none")]
    pub always_apply: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RuleFile {
    pub source: String,
    pub source_display_name: String,
    pub scope: String,
    pub category: String,
    pub filename: String,
    pub path: String,
    pub content: String,
    pub size_bytes: u64,
    pub enabled: bool,
    pub toggleable: bool,
    pub frontmatter: Option<RuleFrontmatter>,
}
