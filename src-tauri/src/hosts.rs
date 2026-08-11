//! Where an agent's data physically lives.
//!
//! A single machine can run Claude Code / Codex / OpenCode twice over: once natively, and once
//! inside one or more WSL distributions, each with its own `$HOME` and its own `~/.claude`. Those
//! are the *same agent*, so they stay one source in the UI; what differs is the **host** the data
//! sits on.
//!
//! The rule that keeps this from leaking everywhere: **every path that crosses a provider boundary
//! is a path this process can actually open.** For WSL that means the `\\wsl.localhost\<distro>\…`
//! UNC form, not the `/home/…` string the agent recorded. Reading project instructions, revealing a
//! folder and opening a terminal then all work off one path, and the distro can be recovered from
//! the path itself ([`Host::of_path`]) instead of being threaded through every call.
//!
//! Only *keys* — project slugs, snapshot names, rule categories — get an explicit `@wsl:<distro>/`
//! prefix, because those are opaque identifiers with no path to read the host from. A distro used
//! by more than one account extends that to `@wsl:<distro>~<user>/`, since the path form cannot
//! distinguish the accounts.

use std::path::{Path, PathBuf};

/// Prefix marking a key (project slug, snapshot name, …) as belonging to a WSL host.
const KEY_PREFIX: &str = "@wsl:";

/// UNC roots Windows exposes WSL filesystems under. `wsl.localhost` is current; `wsl$` is the
/// legacy spelling still produced by older tooling, so both are recognised on the way in.
const UNC_ROOTS: [&str; 2] = ["wsl.localhost", "wsl$"];

/// Separates distro from account inside a host key. Not `/`, which already separates the host from
/// the key it prefixes.
const USER_SEPARATOR: char = '~';

/// Paths, relative to a home, that mean an agent has actually been *used* there.
///
/// Deliberately not `.claude` / `.codex` themselves: those directories get created by a first run
/// that never went anywhere, or left behind by an uninstall, and an empty one is common enough that
/// treating it as a host produces a badge for a store with nothing in it — worse, it can make a
/// distro look multi-account and force `~user` suffixes onto keys for no reason. Each entry below
/// is a store a provider would actually read.
const AGENT_MARKERS: [&str; 7] = [
    ".claude/projects",
    ".claude/history.jsonl",
    ".claude.json",
    ".codex/sessions",
    ".local/share/opencode/opencode.db",
    ".config/opencode/opencode.json",
    ".config/opencode/opencode.jsonc",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Host {
    /// The machine the app itself runs on.
    Native,
    Wsl {
        distro: String,
        /// The account inside the distro, set only when several of them hold agent data. One
        /// distro with one user — the normal case — stays plain `WSL:Ubuntu`, but a distro with
        /// two must keep them apart rather than silently show only one of them.
        user: Option<String>,
    },
}

impl Host {
    /// Short label shown next to sessions and projects. `None` for the native host, which needs no
    /// badge — an unlabelled row means "same machine as the app".
    pub fn tag(&self) -> Option<String> {
        match self {
            Host::Native => None,
            Host::Wsl {
                distro,
                user: Some(user),
            } => Some(format!("WSL:{}/{}", distro, user)),
            Host::Wsl { distro, .. } => Some(format!("WSL:{}", distro)),
        }
    }

    /// Stable identifier used inside composite keys. Never contains `/`, which is what separates
    /// the host from the key it prefixes.
    pub fn key(&self) -> String {
        match self {
            Host::Native => String::new(),
            Host::Wsl {
                distro,
                user: Some(user),
            } => format!("{}{}{}", distro, USER_SEPARATOR, user),
            Host::Wsl { distro, .. } => distro.clone(),
        }
    }

    /// The distro this host lives in, ignoring which account inside it.
    pub fn distro(&self) -> Option<&str> {
        match self {
            Host::Native => None,
            Host::Wsl { distro, .. } => Some(distro),
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self, Host::Native)
    }

    /// Which host a readable path lives on, recovered from the path itself. A path names its
    /// distro but not the account, so the answer is distro-level and the provider registry
    /// resolves it to a concrete store.
    pub fn of_path(path: &Path) -> Host {
        wsl_distro_of(path)
            .map(|distro| Host::Wsl { distro, user: None })
            .unwrap_or(Host::Native)
    }

    /// The UNC root for this host's filesystem (`\\wsl.localhost\<distro>`), or `None` natively.
    pub fn unc_root(&self) -> Option<PathBuf> {
        match self {
            Host::Native => None,
            Host::Wsl { distro, .. } => Some(PathBuf::from(format!(r"\\wsl.localhost\{}", distro))),
        }
    }

    /// Turn a path as the *agent* recorded it (a Linux path, for a WSL host) into one this process
    /// can open. Already-readable paths pass through unchanged.
    pub fn to_readable(&self, recorded: &str) -> PathBuf {
        let Host::Wsl { .. } = self else {
            return PathBuf::from(recorded);
        };
        if wsl_distro_of(Path::new(recorded)).is_some() || recorded.starts_with(r"\\") {
            return PathBuf::from(recorded);
        }
        let Some(root) = self.unc_root() else {
            return PathBuf::from(recorded);
        };
        let relative = recorded.trim_start_matches('/').replace('/', r"\");
        if relative.is_empty() {
            root
        } else {
            PathBuf::from(format!(r"{}\{}", root.to_string_lossy(), relative))
        }
    }

    /// The inverse: the path the agent (and any shell running on that host) uses. For a WSL host
    /// this strips the UNC prefix back to `/home/…`; natively it is the path as-is.
    pub fn to_agent_path(&self, readable: &Path) -> String {
        match self {
            Host::Native => readable.to_string_lossy().to_string(),
            Host::Wsl { .. } => strip_unc(readable)
                .unwrap_or_else(|| readable.to_string_lossy().to_string().replace('\\', "/")),
        }
    }

    /// Decode a Claude Code project slug into the project path it encodes. Slug encoding follows
    /// the machine the agent ran on, not the machine reading it: a WSL slug is always POSIX-shaped
    /// even when a Windows build is the one decoding it.
    pub fn decode_project_slug(&self, slug: &str) -> String {
        match self {
            Host::Native => crate::paths::decode_project_slug(slug),
            Host::Wsl { .. } => {
                let posix = format!("/{}", slug.trim_start_matches('-').replace('-', "/"));
                self.to_readable(&posix).to_string_lossy().to_string()
            }
        }
    }

    /// Tag an opaque key (project slug, snapshot name, rule category) with this host.
    pub fn tag_key(&self, key: &str) -> String {
        match self {
            Host::Native => key.to_string(),
            Host::Wsl { .. } => format!("{}{}/{}", KEY_PREFIX, self.key(), key),
        }
    }
}

/// Split a possibly host-tagged key back into its host and the provider-local key.
pub fn split_key(key: &str) -> (Host, &str) {
    let Some(rest) = key.strip_prefix(KEY_PREFIX) else {
        return (Host::Native, key);
    };
    match rest.split_once('/') {
        Some((host, inner)) if !host.is_empty() => {
            let (distro, user) = match host.split_once(USER_SEPARATOR) {
                Some((distro, user)) if !distro.is_empty() && !user.is_empty() => {
                    (distro, Some(user.to_string()))
                }
                _ => (host, None),
            };
            (
                Host::Wsl {
                    distro: distro.to_string(),
                    user,
                },
                inner,
            )
        }
        _ => (Host::Native, key),
    }
}

/// `\\wsl.localhost\Ubuntu\home\me` → `Ubuntu`. `None` for anything not on a WSL share.
fn wsl_distro_of(path: &Path) -> Option<String> {
    let text = path.to_string_lossy().replace('/', r"\");
    let rest = text.strip_prefix(r"\\")?;
    let mut parts = rest.splitn(3, '\\');
    let root = parts.next()?;
    let distro = parts.next()?;
    if !UNC_ROOTS
        .iter()
        .any(|candidate| root.eq_ignore_ascii_case(candidate))
        || distro.is_empty()
    {
        return None;
    }
    Some(distro.to_string())
}

/// `\\wsl.localhost\Ubuntu\home\me` → `/home/me`.
fn strip_unc(path: &Path) -> Option<String> {
    let text = path.to_string_lossy().replace('/', r"\");
    let rest = text.strip_prefix(r"\\")?;
    let mut parts = rest.splitn(3, '\\');
    parts.next()?;
    parts.next()?;
    let inner = parts.next().unwrap_or("");
    Some(format!("/{}", inner.replace('\\', "/")))
}

/// A discovered place to look for agent data.
#[derive(Debug, Clone)]
pub struct HostHome {
    pub host: Host,
    /// Readable home directory (a UNC path for WSL hosts).
    pub home: PathBuf,
}

/// The homes discovery found, published for the parts of the app that read agent config files
/// directly instead of going through a provider (the Tools page). Empty until discovery runs.
static DISCOVERED: std::sync::OnceLock<std::sync::RwLock<Vec<HostHome>>> =
    std::sync::OnceLock::new();

fn discovered() -> &'static std::sync::RwLock<Vec<HostHome>> {
    DISCOVERED.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

pub fn publish_wsl_homes(homes: &[HostHome]) {
    if let Ok(mut slot) = discovered().write() {
        *slot = homes.to_vec();
    }
}

pub fn known_wsl_homes() -> Vec<HostHome> {
    discovered()
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default()
}

/// Every WSL home that actually holds agent data.
///
/// Reading `\\wsl.localhost\<distro>` boots the distro if it is not already running, so this is
/// deliberately kept off the startup path (see `MultiHostProvider`) and skips distros the user has
/// excluded. Only homes with a recognisable agent directory are returned, so a distro that has
/// never run one of these tools costs a single directory listing and nothing more.
pub fn discover_wsl_homes(excluded: &[String]) -> Vec<HostHome> {
    let mut homes = Vec::new();
    for distro in installed_distros() {
        if excluded
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&distro))
        {
            continue;
        }
        let root = PathBuf::from(format!(r"\\wsl.localhost\{}", distro));
        let found = homes_with_agent_data(&root);
        // Only disambiguate by account when there is something to disambiguate, so the ordinary
        // one-user distro keeps the short `WSL:Ubuntu` label.
        let name_users = found.len() > 1;
        for home in found {
            let user = name_users
                .then(|| {
                    home.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                })
                .flatten();
            homes.push(HostHome {
                host: Host::Wsl {
                    distro: distro.clone(),
                    user,
                },
                home,
            });
        }
    }
    homes
}

/// Every home inside a distro that holds agent data: each `/home/<user>`, plus `/root`.
///
/// All of them are returned rather than the most promising one. Picking a winner would silently
/// hide whichever account lost, and nothing on disk says which one the user meant — a distro used
/// by two accounts is unusual, but losing one of them outright is worse than an extra badge.
fn homes_with_agent_data(root: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(root.join("home"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    candidates.sort();
    candidates.push(root.join("root"));

    candidates
        .into_iter()
        .filter(|home| {
            AGENT_MARKERS
                .iter()
                .any(|marker| has_content(&home.join(marker)))
        })
        .collect()
}

/// True only if the path holds something. An empty `projects/` or a zero-byte `history.jsonl` is
/// what a tool that was installed and never used leaves behind, and surfacing that as a host means
/// an empty badge in the UI.
fn has_content(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.is_dir() {
        return std::fs::read_dir(path)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
    }
    meta.len() > 0
}

#[cfg(windows)]
fn installed_distros() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    /// Keep the packaged GUI app from flashing a console window while listing distros.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(output) = std::process::Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    decode_wsl_list(&output.stdout)
}

#[cfg(not(windows))]
fn installed_distros() -> Vec<String> {
    Vec::new()
}

/// `wsl.exe --list` writes UTF-16LE, so its output cannot go through `String::from_utf8`.
fn decode_wsl_list(raw: &[u8]) -> Vec<String> {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|line| line.trim().trim_matches('\u{feff}').trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ubuntu() -> Host {
        Host::Wsl {
            distro: "Ubuntu".to_string(),
            user: None,
        }
    }

    #[test]
    fn recorded_linux_paths_become_readable_unc_paths_and_back() {
        let host = ubuntu();
        let readable = host.to_readable("/home/me/code/app");

        assert_eq!(
            readable,
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\me\code\app")
        );
        assert_eq!(host.to_agent_path(&readable), "/home/me/code/app");
        // Already-readable paths must survive a second translation unchanged.
        assert_eq!(host.to_readable(&readable.to_string_lossy()), readable);
    }

    #[test]
    fn host_is_recoverable_from_a_path_including_the_legacy_unc_spelling() {
        assert_eq!(
            Host::of_path(Path::new(r"\\wsl.localhost\Ubuntu\home\me")),
            ubuntu()
        );
        assert_eq!(Host::of_path(Path::new(r"\\wsl$\Ubuntu\home\me")), ubuntu());
        assert_eq!(Host::of_path(Path::new(r"C:\Users\me")), Host::Native);
        assert_eq!(Host::of_path(Path::new("/home/me")), Host::Native);
        // A plain network share is not a WSL host.
        assert_eq!(
            Host::of_path(Path::new(r"\\fileserver\share\me")),
            Host::Native
        );
    }

    #[test]
    fn wsl_project_slugs_decode_posix_style_regardless_of_the_reading_platform() {
        assert_eq!(
            ubuntu().decode_project_slug("-home-me-code-app"),
            r"\\wsl.localhost\Ubuntu\home\me\code\app"
        );
    }

    #[test]
    fn tagged_keys_round_trip_and_untagged_keys_stay_native() {
        let tagged = ubuntu().tag_key("-home-me-code-app");

        assert_eq!(tagged, "@wsl:Ubuntu/-home-me-code-app");
        assert_eq!(split_key(&tagged), (ubuntu(), "-home-me-code-app"));
        assert_eq!(split_key("C--Codes-app"), (Host::Native, "C--Codes-app"));
        assert_eq!(Host::Native.tag_key("C--Codes-app"), "C--Codes-app");
        // A malformed tag must not swallow the key.
        assert_eq!(split_key("@wsl:"), (Host::Native, "@wsl:"));
    }

    #[test]
    fn two_accounts_in_one_distro_stay_distinct_keys() {
        let caoji = Host::Wsl {
            distro: "Ubuntu-24.04".to_string(),
            user: Some("caoji".to_string()),
        };
        let codex = Host::Wsl {
            distro: "Ubuntu-24.04".to_string(),
            user: Some("codex".to_string()),
        };

        assert_eq!(caoji.tag().as_deref(), Some("WSL:Ubuntu-24.04/caoji"));
        assert_ne!(caoji.tag_key("proj"), codex.tag_key("proj"));
        assert_eq!(split_key(&caoji.tag_key("proj")), (caoji.clone(), "proj"));
        assert_eq!(split_key(&codex.tag_key("proj")), (codex, "proj"));
        // Both accounts still resolve to the one distro, which is all a path can tell us.
        assert_eq!(caoji.distro(), Some("Ubuntu-24.04"));
        assert_eq!(
            Host::of_path(Path::new(r"\\wsl.localhost\Ubuntu-24.04\home\caoji\app")).distro(),
            Some("Ubuntu-24.04")
        );
    }

    #[test]
    fn wsl_list_output_is_decoded_from_utf16() {
        let mut raw = Vec::new();
        for unit in "Ubuntu\r\nDebian\r\n".encode_utf16() {
            raw.extend_from_slice(&unit.to_le_bytes());
        }

        assert_eq!(decode_wsl_list(&raw), vec!["Ubuntu", "Debian"]);
    }
}
