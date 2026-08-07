use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionModelInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

impl SessionModelInfo {
    pub fn new(
        provider: Option<String>,
        model: Option<String>,
        thinking_level: Option<String>,
    ) -> Option<Self> {
        let info = Self {
            provider: clean(provider),
            model: clean(model),
            thinking_level: clean(thinking_level),
        };
        if info.provider.is_some() || info.model.is_some() || info.thinking_level.is_some() {
            Some(info)
        } else {
            None
        }
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn push_model_context(
    contexts: &mut Vec<SessionModelInfo>,
    provider: Option<String>,
    model: Option<String>,
    thinking_level: Option<String>,
) {
    if let Some(info) = SessionModelInfo::new(provider, model, thinking_level) {
        if !contexts.contains(&info) {
            contexts.push(info);
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    /// Which coding agent this session belongs to ("claude" | "codex" | …).
    pub source: String,
    pub session_id: String,
    pub project: String,
    pub project_path: String,
    /// The first real user prompt. This is intentionally distinct from the agent-generated title.
    pub first_prompt: Option<String>,
    /// Native title / summary assigned by the coding agent (when that agent persists one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_title: Option<String>,
    /// Time at which the session was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Time of the most recent interaction in the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Backward-compatible alias for `updated_at`.
    pub timestamp: Option<String>,
    pub file_size_bytes: u64,
    pub subagent_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_contexts: Vec<SessionModelInfo>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SessionRecord {
    pub record_type: String,
    pub content_preview: String,
    pub timestamp: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub diff: Option<DiffInfo>,
    pub level: String,
    /// Byte offset of the source line in the .jsonl file (for scroll/anchor preservation).
    #[serde(default)]
    pub byte_offset: u64,
    /// The assistant message id this record came from. Multiple tool_use records sharing a
    /// group_id were issued together (one model turn) — i.e. a parallel tool-call batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Structured result payload for interactive tools (e.g. AskUserQuestion `answers` map).
    /// Only populated on tool_result records; None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_meta: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiffInfo {
    pub file_path: String,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct PaginatedRecords {
    pub records: Vec<SessionRecord>,
    /// Inclusive cursor for the first source unit represented by this page. Pass this value to
    /// `get_session_before` to fetch the preceding page without repeating the boundary record.
    pub start_byte_offset: u64,
    /// Exclusive cursor immediately after the source units scanned for this page. Pass this value
    /// to `get_session_detail` to continue forwards.
    pub next_byte_offset: u64,
    /// Whether another page exists before `start_byte_offset` at the selected display level.
    pub has_earlier: bool,
    /// Whether source data exists after `next_byte_offset`.
    pub has_more: bool,
}

impl PaginatedRecords {
    /// Terminal tools persist raw ANSI/VT control sequences in session archives. They are useful
    /// to a real terminal emulator but render as fragments such as `[32;1m` in plain `<pre>` text.
    /// Strip presentation controls at the response boundary while preserving the actual output.
    pub fn without_terminal_formatting(mut self) -> Self {
        for record in &mut self.records {
            // Only tool results are command output candidates. User/assistant text may discuss or
            // quote ANSI sequences literally and must remain byte-for-byte readable.
            if record.record_type == "tool_result"
                && (record.content_preview.contains('\u{001b}')
                    || record.content_preview.contains('\u{009b}')
                    || record.content_preview.contains('\r'))
            {
                record.content_preview = strip_terminal_formatting(&record.content_preview);
            }
            if let Some(serde_json::Value::String(stderr)) = record
                .result_meta
                .as_mut()
                .and_then(|meta| meta.pointer_mut("/terminal/stderr"))
            {
                *stderr = strip_terminal_formatting(stderr);
            }
        }
        self
    }
}

fn strip_terminal_formatting(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            // ESC-prefixed CSI (`ESC [`), OSC (`ESC ]`) and short VT escape sequences.
            '\u{001b}' => match chars.next() {
                Some('[') => consume_csi(&mut chars),
                Some(']') => consume_osc(&mut chars),
                Some(_) | None => {}
            },
            // Eight-bit CSI form.
            '\u{009b}' => consume_csi(&mut chars),
            // CR is presentation-only here. Removing it also normalizes CRLF without changing LF.
            '\r' => {}
            _ => output.push(character),
        }
    }
    output
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for character in chars.by_ref() {
        if ('\u{0040}'..='\u{007e}').contains(&character) {
            break;
        }
    }
}

fn consume_osc(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(character) = chars.next() {
        if character == '\u{0007}' {
            break;
        }
        if character == '\u{001b}' && chars.next_if_eq(&'\\').is_some() {
            break;
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SessionSearchHit {
    pub byte_offset: u64,
    pub snippet: String,
    pub record_type: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubagentInfo {
    pub agent_id: String,
    pub agent_type: String,
    pub description: String,
    pub tool_use_id: String,
    pub record_count: u32,
}

#[derive(Debug, Deserialize)]
pub struct SubagentMeta {
    #[serde(rename = "agentType")]
    pub agent_type: String,
    pub description: String,
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
}

#[cfg(test)]
mod tests {
    use super::{strip_terminal_formatting, PaginatedRecords, SessionRecord};

    #[test]
    fn strips_ansi_tables_and_osc_links_without_losing_text() {
        let input = concat!(
            "\u{001b}[32;1m Id\u{001b}[0m \u{001b}[32;1mResponding\u{001b}[0m\r\n",
            "50416 True ",
            "\u{001b}]8;;file:///tmp/app\u{0007}/tmp/app\u{001b}]8;;\u{0007}\n",
        );
        assert_eq!(
            strip_terminal_formatting(input),
            " Id Responding\n50416 True /tmp/app\n"
        );
    }

    #[test]
    fn strips_eight_bit_csi() {
        assert_eq!(
            strip_terminal_formatting("\u{009b}31merror\u{009b}0m"),
            "error"
        );
    }

    #[test]
    fn sanitizes_tool_results_but_preserves_literal_user_examples() {
        let record = |record_type: &str| SessionRecord {
            record_type: record_type.to_string(),
            content_preview: "literal \u{001b}[32;1mgreen\u{001b}[0m".to_string(),
            timestamp: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            diff: None,
            level: "content".to_string(),
            byte_offset: 0,
            group_id: None,
            result_meta: None,
        };
        let page = PaginatedRecords {
            records: vec![record("user"), record("tool_result")],
            start_byte_offset: 0,
            next_byte_offset: 0,
            has_earlier: false,
            has_more: false,
        }
        .without_terminal_formatting();

        assert_eq!(
            page.records[0].content_preview,
            "literal \u{001b}[32;1mgreen\u{001b}[0m"
        );
        assert_eq!(page.records[1].content_preview, "literal green");
    }
}
