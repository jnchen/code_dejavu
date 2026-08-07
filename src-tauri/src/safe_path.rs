//! Path-component validation for untrusted IPC inputs (project slugs, file names, rule categories).
//!
//! The memory/rule write commands join user-supplied strings onto a base directory
//! (`dir.join(project).join(filename)`). Without validation, a value like
//! `../../../.ssh/authorized_keys` — or an absolute path — would escape the intended directory.
//! Combined with the markdown render surface, that turned into an "arbitrary path write" primitive.
//! These helpers reject anything that could navigate outside its base BEFORE it reaches
//! `Path::join`, and live here so every command validates the same way.

use crate::error::AppError;

fn rejected(label: &str, value: &str) -> AppError {
    AppError::Archive(format!("非法的{}：{:?}", label, value))
}

/// Validate a single untrusted path *segment* (a project slug or a file name): it must be a plain
/// name with no path navigation. Rejects empty, `.`, `..`, any `/` or `\`, NUL bytes, and
/// absolute / drive-relative forms.
pub fn validate_segment(label: &str, value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        return Err(rejected(label, value));
    }
    Ok(())
}

/// Validate an untrusted *relative* path that may legitimately contain nested separators (e.g. a
/// rule category such as `frontend/ui`). Rejects absolute / drive-relative / UNC paths and any
/// empty, `.` or `..` component, so the joined result can never escape its base directory.
pub fn validate_relative(label: &str, value: &str) -> Result<(), AppError> {
    if value.is_empty() || value.contains('\0') {
        return Err(rejected(label, value));
    }
    // POSIX absolute ("/...") or Windows root / UNC ("\...").
    if value.starts_with('/') || value.starts_with('\\') {
        return Err(rejected(label, value));
    }
    // Windows drive-relative ("C:..").
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Err(rejected(label, value));
    }
    for segment in value.split(['/', '\\']) {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(rejected(label, value));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_accepts_plain_names() {
        assert!(validate_segment("filename", "MEMORY.md").is_ok());
        assert!(validate_segment("filename", "project-memory-1.md").is_ok());
        assert!(validate_segment("project", "-Users-foo-bar").is_ok());
    }

    #[test]
    fn segment_rejects_traversal_and_separators() {
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "/etc/passwd", "x\0y"] {
            assert!(
                validate_segment("x", bad).is_err(),
                "should reject {:?}",
                bad
            );
        }
    }

    #[test]
    fn relative_accepts_nested_categories() {
        assert!(validate_relative("category", "root").is_ok());
        assert!(validate_relative("category", "frontend/ui").is_ok());
        assert!(validate_relative("category", "a/b/c").is_ok());
    }

    #[test]
    fn relative_rejects_escapes() {
        for bad in [
            "",
            "/abs",
            "\\abs",
            "C:/x",
            "../etc",
            "a/../../b",
            "a/./b",
            "a/b/..",
            "x\0y",
        ] {
            assert!(
                validate_relative("category", bad).is_err(),
                "should reject {:?}",
                bad
            );
        }
    }
}
