use std::path::PathBuf;

pub fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("USERPROFILE")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".local")
                    .join("share")
            })
            .join("CodeDejavu")
    }

    #[cfg(not(windows))]
    {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".local")
                    .join("share")
            })
            .join("code-dejavu")
    }
}

/// Full map of the Claude config layout. Some entries (plans/skills/tasks/backups/
/// session-data) are intentionally enumerated for completeness and future use even
/// though not every consumer reads them yet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ClaudePaths {
    pub claude_dir: PathBuf,
    pub claude_json: PathBuf,
    pub credentials: PathBuf,
    pub archive_root: PathBuf,
    pub claude_md: PathBuf,
    pub settings_json: PathBuf,
    pub rules_dir: PathBuf,
    pub projects_dir: PathBuf,
    pub session_data_dir: PathBuf,
    pub history_jsonl: PathBuf,
    pub plans_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl ClaudePaths {
    pub fn new() -> Self {
        Self::for_home(&Self::home_dir())
    }

    /// The same layout rooted at an arbitrary home directory. Lets tests work against a temp home,
    /// and lets a Claude install that lives in another home (e.g. inside a WSL distro) be addressed
    /// without the process's own `$HOME` being involved.
    pub fn for_home(home: &std::path::Path) -> Self {
        let claude_dir = home.join(".claude");

        Self {
            claude_json: home.join(".claude.json"),
            credentials: claude_dir.join(".credentials.json"),
            archive_root: claude_dir.join("_archives"),
            claude_md: claude_dir.join("CLAUDE.md"),
            settings_json: claude_dir.join("settings.json"),
            rules_dir: claude_dir.join("rules"),
            projects_dir: claude_dir.join("projects"),
            session_data_dir: claude_dir.join("session-data"),
            history_jsonl: claude_dir.join("history.jsonl"),
            plans_dir: claude_dir.join("plans"),
            skills_dir: claude_dir.join("skills"),
            tasks_dir: claude_dir.join("tasks"),
            backups_dir: claude_dir.join("backups"),
            claude_dir,
        }
    }

    fn home_dir() -> PathBuf {
        #[cfg(windows)]
        {
            std::env::var("USERPROFILE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("C:\\Users\\Default"))
        }
        #[cfg(not(windows))]
        {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
        }
    }
}

pub fn decode_project_slug(slug: &str) -> String {
    #[cfg(windows)]
    {
        if slug.len() >= 3 && slug.as_bytes()[1] == b'-' && slug.as_bytes()[2] == b'-' {
            let drive = &slug[0..1];
            let rest = &slug[3..];
            let path = rest.replace('-', "\\");
            format!("{}:\\{}", drive.to_uppercase(), path)
        } else {
            slug.replace('-', "\\")
        }
    }
    #[cfg(not(windows))]
    {
        format!("/{}", slug.replace('-', "/"))
    }
}
