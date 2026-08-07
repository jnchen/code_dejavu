pub fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content.to_string());
    }

    let after_first = &trimmed[3..];
    let after_first = after_first.trim_start_matches(['\r', '\n']);

    if let Some(end_pos) = after_first.find("\n---") {
        let yaml = &after_first[..end_pos];
        let body_start = end_pos + 4; // "\n---"
        let body = after_first[body_start..]
            .trim_start_matches(['\r', '\n'])
            .to_string();
        (Some(yaml.to_string()), body)
    } else {
        (None, content.to_string())
    }
}

pub fn join_frontmatter(yaml: &str, body: &str) -> String {
    format!("---\n{}\n---\n\n{}", yaml.trim(), body)
}
