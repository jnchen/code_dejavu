//! Machine-dependent smoke checks, all `#[ignore]`d.
//!
//! These exercise the real filesystem on the developer's box — the WSL shares that actually exist,
//! and archive timings on a synthetic-but-realistic session tree. They cannot run in CI (there is
//! no WSL, and timings are meaningless there), so they are opt-in:
//!
//! ```text
//! cargo test --lib smoke -- --ignored --nocapture
//! ```

#![cfg(test)]

use crate::agents::AgentProvider;
#[cfg(windows)]
use crate::agents::{ClaudeProvider, CodexProvider, MultiHostProvider};
#[cfg(windows)]
use crate::hosts::{self, Host};
use crate::paths::ClaudePaths;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Arc;
use std::time::Instant;

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    println!("  [{:>8.0?}] {}", start.elapsed(), label);
    value
}

/// Report what discovery sees on this machine. Not an assertion about *how many* homes exist —
/// that depends on the box — but it does assert the thing that is always true: a directory an
/// agent merely created must not be mistaken for one it uses.
#[cfg(windows)]
#[test]
#[ignore = "reads the real WSL shares on this machine"]
fn wsl_discovery_reports_real_agent_homes() {
    let homes = timed("discover_wsl_homes", || hosts::discover_wsl_homes(&[]));
    println!("discovered {} home(s) with real agent data", homes.len());
    for home in &homes {
        println!(
            "  host={:<28} key={:<24} home={}",
            home.host.tag().unwrap_or_else(|| "<native>".into()),
            home.host.key(),
            home.home.display()
        );
    }
    for home in &homes {
        assert!(
            AGENT_MARKER_PROBES
                .iter()
                .any(|marker| home.home.join(marker).exists()),
            "{} was surfaced without any agent store in it",
            home.home.display()
        );
    }
}

#[cfg(windows)]
const AGENT_MARKER_PROBES: [&str; 7] = [
    ".claude/projects",
    ".claude/history.jsonl",
    ".claude.json",
    ".codex/sessions",
    ".local/share/opencode/opencode.db",
    ".config/opencode/opencode.json",
    ".config/opencode/opencode.jsonc",
];

/// The first WSL distribution installed on this machine, or `None`.
#[cfg(windows)]
fn any_distro() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    let units: Vec<u16> = output
        .stdout
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|line| line.trim().trim_matches('\u{feff}').trim().to_string())
        .find(|line| !line.is_empty())
}

/// Run a command inside the distro and return its stdout.
#[cfg(windows)]
fn wsl_capture(distro: &str, script: &str) -> String {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("wsl.exe")
        .args(["-d", distro, "--", "bash", "-lc", script])
        .creation_flags(0x0800_0000)
        .output()
        .expect("run wsl.exe");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A throwaway agent home planted inside the distro, wiped on drop.
///
/// It lives under the default user's `$HOME`, **not** `/tmp`: the `\\wsl.localhost` share and a
/// shell inside the distro do not agree about `/tmp` (it is wiped when the distro restarts, and a
/// share access can itself restart a stopped distro), so a fixture there can vanish between being
/// written from Windows and being read from bash. `$HOME` is also where real agent data lives and
/// the only place discovery looks, so it is the honest location to test against.
#[cfg(windows)]
struct WslFixture {
    distro: String,
    /// Path as the distro sees it, e.g. `/home/me/.dejavu-smoke-1234`.
    posix_home: String,
    /// The same directory as this process must open it.
    home: PathBuf,
}

#[cfg(windows)]
impl WslFixture {
    fn new(distro: String) -> Self {
        let host = Host::Wsl {
            distro: distro.clone(),
            user: None,
        };
        let distro_home = wsl_capture(&distro, "echo $HOME");
        assert!(
            distro_home.starts_with('/'),
            "could not resolve $HOME inside {}: {:?}",
            distro,
            distro_home
        );
        let posix_home = format!(
            "{}/.dejavu-smoke-{}",
            distro_home.trim_end_matches('/'),
            std::process::id()
        );
        let home = host.to_readable(&posix_home);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create fixture home inside the distro");
        Self {
            distro,
            posix_home,
            home,
        }
    }

    fn host(&self) -> Host {
        Host::Wsl {
            distro: self.distro.clone(),
            user: None,
        }
    }

    fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.home.join(relative.replace('/', "\\"));
        fs::create_dir_all(path.parent().expect("parent")).expect("fixture dir");
        fs::write(&path, body).expect("fixture file");
        path
    }
}

#[cfg(windows)]
impl Drop for WslFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// Plant real session files inside a WSL distro and read them back through the provider stack the
/// app actually builds.
///
/// This is the part that cannot be proven with a temp directory on `C:`: the paths are genuine
/// `\\wsl.localhost` UNC paths, the recorded `cwd` values are genuine POSIX paths, and the project
/// slugs are encoded the way Claude Code encodes them on Linux.
#[cfg(windows)]
#[test]
#[ignore = "writes a fixture into this machine's WSL /tmp, then removes it"]
fn wsl_fixture_is_read_end_to_end_through_the_provider_stack() {
    let Some(distro) = any_distro() else {
        panic!("no WSL distribution on this machine");
    };
    println!("distro = {}", distro);
    let fixture = WslFixture::new(distro.clone());
    println!("fixture home = {}", fixture.home.display());
    println!("  as WSL sees it = {}", fixture.posix_home);

    // A project directory the sessions claim to have run in, with a project instruction file.
    let project_posix = format!("{}/demo-project", fixture.posix_home);
    fixture.write("demo-project/CLAUDE.md", "# demo project rules\n");

    // --- Claude Code: slug-encoded project dir + a JSONL session recording a POSIX cwd ---
    let slug = project_posix.replace('/', "-");
    let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let claude_session = format!(
        concat!(
            r#"{{"type":"mode","mode":"normal","sessionId":"{id}"}}"#,
            "\n",
            r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"wsl fixture prompt"}},"uuid":"11111111-1111-1111-1111-111111111111","timestamp":"2026-08-08T06:05:16.036Z","cwd":"{cwd}","sessionId":"{id}","version":"2.1.223","gitBranch":"main"}}"#,
            "\n",
            r#"{{"parentUuid":"11111111-1111-1111-1111-111111111111","isSidechain":false,"type":"assistant","message":{{"role":"assistant","model":"claude-opus-5","content":[{{"type":"text","text":"wsl fixture reply"}}],"usage":{{"input_tokens":10,"output_tokens":5}}}},"uuid":"22222222-2222-2222-2222-222222222222","timestamp":"2026-08-08T06:05:20.000Z","cwd":"{cwd}","sessionId":"{id}"}}"#,
            "\n"
        ),
        id = session_id,
        cwd = project_posix
    );
    fixture.write(
        &format!(".claude/projects/{}/{}.jsonl", slug, session_id),
        &claude_session,
    );
    fixture.write(".claude/CLAUDE.md", "# global wsl rules\n");

    // --- Codex: a date-bucketed rollout recording the same POSIX cwd ---
    let thread_id = "019fd0f7-0000-7000-8000-000000000001";
    let rollout = format!(
        concat!(
            r#"{{"timestamp":"2026-08-08T06:05:00.000Z","type":"session_meta","payload":{{"session_id":"{id}","id":"{id}","timestamp":"2026-08-08T06:05:00.000Z","cwd":"{cwd}","originator":"codex-tui","cli_version":"0.146.0","source":"cli"}}}}"#,
            "\n",
            r#"{{"timestamp":"2026-08-08T06:05:01.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"codex wsl fixture prompt"}}]}}}}"#,
            "\n",
            r#"{{"timestamp":"2026-08-08T06:05:02.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"codex wsl fixture reply"}}]}}}}"#,
            "\n"
        ),
        id = thread_id,
        cwd = project_posix
    );
    fixture.write(
        &format!(
            ".codex/sessions/2026/08/08/rollout-2026-08-08T06-05-00-{}.jsonl",
            thread_id
        ),
        &rollout,
    );

    let home = fixture.home.clone();
    let host = fixture.host();

    // Exactly the stack `run()` builds, with the fixture adopted as an extra host.
    let claude = Arc::new(MultiHostProvider::new(
        Arc::new(ClaudeProvider::new(ClaudePaths::for_home(Path::new(
            "C:\\nonexistent-native-home",
        )))),
        Box::new(|host, home| {
            Arc::new(ClaudeProvider::for_host(host, ClaudePaths::for_home(home)))
        }),
    ));
    let codex = Arc::new(MultiHostProvider::new(
        Arc::new(CodexProvider::for_host(
            Host::Native,
            Path::new("C:\\nonexistent-native-home"),
        )),
        Box::new(|host, home| Arc::new(CodexProvider::for_host(host, home))),
    ));
    let adopted = [hosts::HostHome {
        host: host.clone(),
        home: home.clone(),
    }];
    claude.adopt(&adopted);
    codex.adopt(&adopted);

    let expected_tag = format!("WSL:{}", distro);
    assert_eq!(claude.hosts(), vec![expected_tag.clone()]);

    // ---- Claude ----
    println!("\n=== Claude Code ===");
    let sessions = timed("list_sessions(None)", || {
        claude.list_sessions(None).expect("sessions")
    });
    assert_eq!(sessions.len(), 1, "expected exactly the fixture session");
    let session = &sessions[0];
    println!("  project      = {}", session.project);
    println!("  project_path = {}", session.project_path);
    println!("  first_prompt = {:?}", session.first_prompt);

    // The project key is host-tagged; the project path is readable and points at the real dir.
    assert_eq!(session.project, host.tag_key(&slug));
    assert_eq!(
        session.project_path,
        host.to_readable(&project_posix).to_string_lossy()
    );
    assert!(
        Path::new(&session.project_path).join("CLAUDE.md").exists(),
        "project path from a WSL session is not openable"
    );

    // The tagged key must route back to this session's content.
    let detail = timed("session_tail", || {
        claude
            .session_tail(&session.project, session_id, 50, "content", None)
            .expect("tail")
    });
    let texts: Vec<String> = detail
        .records
        .iter()
        .map(|record| format!("{:?}", record))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("wsl fixture prompt")),
        "session content did not come back through the tagged key: {:?}",
        texts
    );

    // Project instruction discovery has to reach into the distro.
    let roots = claude.instruction_project_roots();
    println!("  instruction roots = {:?}", roots);
    assert!(
        roots
            .iter()
            .any(|root| root == &host.to_readable(&project_posix)),
        "project root inside WSL was not discovered"
    );
    let candidates = claude.project_instruction_candidates(&host.to_readable(&project_posix));
    let project_md = candidates
        .iter()
        .find(|candidate| candidate.path.file_name().is_some_and(|n| n == "CLAUDE.md"))
        .expect("project CLAUDE.md candidate");
    assert_eq!(
        claude
            .read_instruction_candidate(project_md)
            .expect("read project instructions"),
        "# demo project rules\n"
    );

    // Global candidates are labelled with the host so two "全局 CLAUDE.md" stay tellable apart.
    let global = claude.global_instruction_candidates();
    println!(
        "  global candidates = {:?}",
        global.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    assert!(
        global.iter().any(|c| c.title.contains(&expected_tag)),
        "global instruction candidate was not tagged with its host"
    );

    // Indexing: key and project tagged, incremental reparse routed to the right store.
    let batch = claude.index_documents();
    assert_eq!(batch.docs.len(), 1);
    assert!(batch.docs[0].key.starts_with("@wsl:"));
    let manifest: Vec<String> = claude
        .index_manifest()
        .into_iter()
        .map(|entry| entry.key)
        .collect();
    println!("  manifest = {:?}", manifest);
    let partial = claude.index_documents_for(&manifest.iter().cloned().collect());
    assert_eq!(partial.docs.len(), 1, "incremental reparse lost the doc");

    // ---- Codex ----
    println!("\n=== Codex CLI ===");
    let codex_sessions = timed("list_sessions(None)", || {
        codex.list_sessions(None).expect("sessions")
    });
    println!(
        "  sessions = {:?}",
        codex_sessions
            .iter()
            .map(|s| (&s.project, &s.project_path))
            .collect::<Vec<_>>()
    );
    assert_eq!(codex_sessions.len(), 1);
    let codex_session = &codex_sessions[0];
    assert_eq!(codex_session.project, host.tag_key(&project_posix));
    assert_eq!(
        codex_session.project_path,
        host.to_readable(&project_posix).to_string_lossy()
    );
    let codex_detail = codex
        .session_tail(&codex_session.project, thread_id, 50, "content", None)
        .expect("codex tail");
    let codex_texts = format!("{:?}", codex_detail.records);
    assert!(
        codex_texts.contains("codex wsl fixture prompt"),
        "codex session content did not come back: {}",
        codex_texts
    );

    println!("\nall WSL fixture assertions passed");
}

/// The real script builder, executed inside the distro, output captured.
///
/// The project directory deliberately contains a space and a single quote — the two things naive
/// quoting gets wrong — and an env var is set from the app config, so a shredded command line shows
/// up as a failure rather than as a subtly empty variable.
#[cfg(windows)]
#[test]
#[ignore = "runs wsl.exe on this machine"]
fn wsl_shell_script_runs_intact_inside_the_distro() {
    use crate::commands::shell::{wsl_shell_script, DejavuConfig};
    use std::os::windows::process::CommandExt;

    let Some(distro) = any_distro() else {
        panic!("no WSL distribution on this machine");
    };
    let fixture = WslFixture::new(distro.clone());
    let project = "demo project's code";
    fixture.write(&format!("{}/marker.txt", project), "here");
    let host = fixture.host();

    // Start from the readable path, exactly as it would arrive from a session summary.
    let readable = host.to_readable(&format!("{}/{}", fixture.posix_home, project));
    let cwd = host.to_agent_path(&readable);
    println!("readable = {}", readable.display());
    println!("wsl cwd  = {}", cwd);
    assert_eq!(cwd, format!("{}/{}", fixture.posix_home, project));

    let mut config = DejavuConfig::default();
    config
        .env
        .insert("DEJAVU_SMOKE".to_string(), "it's ok".to_string());
    let script = wsl_shell_script(
        &config,
        &cwd,
        Some("pwd && ls marker.txt && printf 'env=[%s]\\n' \"$DEJAVU_SMOKE\"".to_string()),
        false,
    );
    println!("script   = {}", script);

    let output = std::process::Command::new("wsl.exe")
        .args(["-d", &distro, "-e", "bash", "-lc", &script])
        .creation_flags(0x0800_0000)
        .output()
        .expect("run wsl.exe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("status   = {}", output.status);
    println!("stdout   =\n{}", stdout);
    if !stderr.trim().is_empty() {
        println!("stderr   =\n{}", stderr);
    }

    assert!(output.status.success(), "wsl.exe failed: {}", stderr);
    assert!(
        stdout.contains(&cwd),
        "shell did not land in the project directory"
    );
    assert!(
        stdout.contains("marker.txt"),
        "project contents not visible"
    );
    assert!(
        stdout.contains("env=[it's ok]"),
        "env var from the app config did not survive the command line"
    );
}

/// The full launch path — `cmd /c start wsl.exe …` — really runs the script.
///
/// A started terminal writes its own window; nothing can be captured from it, so the script drops
/// a marker file the test then reads back over the share. That is what proves the extra `cmd` and
/// `start` parsing layers do not shred the command the way `--` did.
#[cfg(windows)]
#[test]
#[ignore = "opens a real terminal window on this machine"]
fn wsl_terminal_launch_path_executes_the_script() {
    use crate::commands::shell::{wsl_shell_script, DejavuConfig};
    use std::process::Command;
    use std::time::Duration;

    let Some(distro) = any_distro() else {
        panic!("no WSL distribution on this machine");
    };
    let fixture = WslFixture::new(distro.clone());
    let project = "launch project";
    fixture.write(&format!("{}/.keep", project), "");
    let host = fixture.host();
    let cwd = host.to_agent_path(&host.to_readable(&format!("{}/{}", fixture.posix_home, project)));

    let mut config = DejavuConfig::default();
    config
        .env
        .insert("DEJAVU_SMOKE".to_string(), "launched".to_string());
    // `keep_open: false` so the window closes itself instead of waiting for a human.
    let script = wsl_shell_script(
        &config,
        &cwd,
        Some("printf '%s|%s\\n' \"$PWD\" \"$DEJAVU_SMOKE\" > launched.txt".to_string()),
        false,
    );
    println!("script = {}", script);

    Command::new("cmd")
        .args([
            "/c", "start", "", "wsl.exe", "-d", &distro, "-e", "bash", "-lc",
        ])
        .arg(&script)
        .spawn()
        .expect("spawn terminal");

    let marker = host
        .to_readable(&format!("{}/{}", fixture.posix_home, project))
        .join("launched.txt");
    let mut body = String::new();
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(text) = fs::read_to_string(&marker) {
            if !text.trim().is_empty() {
                body = text.trim().to_string();
                break;
            }
        }
    }
    println!("marker = {:?}", body);
    assert_eq!(
        body,
        format!("{}|launched", cwd),
        "the launched terminal did not run the script in the right directory"
    );
}

// ---------------------------------------------------------------------------
// Archive timing
// ---------------------------------------------------------------------------

/// Build a tree shaped like a real `~/.claude/projects`: many project dirs, each with a handful of
/// session files, most small and a few large.
fn seed_projects(root: &Path, projects: usize, per_project: usize) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut files = 0usize;
    let small_kb: usize = std::env::var("DEJAVU_BENCH_SMALL_KB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let large_mb: usize = std::env::var("DEJAVU_BENCH_LARGE_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let small = "x".repeat(small_kb * 1024);
    let large = "x".repeat(large_mb * 1024 * 1024);
    for project in 0..projects {
        let dir = root.join(format!("C--Codes-project-{:03}", project));
        fs::create_dir_all(&dir).expect("project dir");
        for session in 0..per_project {
            let body = if session == 0 { &large } else { &small };
            let path = dir.join(format!("session-{:03}.jsonl", session));
            fs::write(&path, body).expect("session file");
            bytes += body.len() as u64;
            files += 1;
        }
    }
    (bytes, files)
}

/// What the archiver used to do: size-walk, serial copy, two verification walks, then delete.
fn legacy_archive(src: &Path, dst: &Path) {
    fn dir_size(path: &Path) -> u64 {
        walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum()
    }
    fn count(path: &Path) -> usize {
        walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count()
    }
    fn copy_dir(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("mkdir");
        for entry in fs::read_dir(src).expect("read_dir").flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir(&from, &to);
            } else {
                fs::copy(&from, &to).expect("copy");
            }
        }
    }

    let _ = dir_size(src);
    copy_dir(src, dst);
    assert_eq!(count(src), count(dst));
    fs::remove_dir_all(src).expect("remove");
}

#[test]
#[ignore = "timing benchmark; writes ~1 GB to the temp volume"]
fn archive_move_versus_legacy_copy() {
    use crate::services::fsops;

    let base = std::env::temp_dir().join(format!("dejavu-archive-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);

    // Sized after a real store on this machine: ~500 rollout files totalling several GB, i.e.
    // large files rather than many tiny ones. Overridable so the same test can be pointed at a
    // lighter or heavier shape.
    let projects: usize = std::env::var("DEJAVU_BENCH_PROJECTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let per_project: usize = std::env::var("DEJAVU_BENCH_PER_PROJECT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    // --- old behaviour ---
    let legacy_src = base.join("legacy").join("projects");
    let legacy_dst = base.join("legacy").join("_archives").join("snap");
    let (bytes, files) = seed_projects(&legacy_src, projects, per_project);
    println!(
        "\ntree: {} files, {:.0} MB across {} project dirs",
        files,
        bytes as f64 / 1024.0 / 1024.0,
        projects
    );
    let legacy_start = Instant::now();
    legacy_archive(&legacy_src, &legacy_dst);
    let legacy_elapsed = legacy_start.elapsed();
    println!("legacy copy+verify+delete : {:?}", legacy_elapsed);

    // --- new behaviour, same volume (the real case: _archives sits inside .claude) ---
    let move_src = base.join("moved").join("projects");
    let move_dst = base.join("moved").join("_archives").join("snap");
    seed_projects(&move_src, projects, per_project);
    let move_start = Instant::now();
    let moved = fsops::move_tree(&move_src, &move_dst, &[]).expect("move");
    let move_elapsed = move_start.elapsed();
    println!("new move (same volume)    : {:?}", move_elapsed);

    assert_eq!(moved.files as usize, files);
    assert_eq!(moved.bytes, bytes);
    assert!(!move_src.exists(), "source should be gone after a move");
    assert!(
        move_dst
            .join("C--Codes-project-000")
            .join("session-000.jsonl")
            .exists(),
        "archived tree is missing its contents"
    );

    // --- new behaviour forced down the copy fallback (cross-volume case) ---
    let copy_src = base.join("copied").join("projects");
    let copy_dst = base.join("copied").join("_archives").join("snap");
    seed_projects(&copy_src, projects, per_project);
    let copy_start = Instant::now();
    let copied = fsops::copy_tree(&copy_src, &copy_dst, &[]).expect("copy");
    let copy_elapsed = copy_start.elapsed();
    println!("new parallel copy fallback: {:?}", copy_elapsed);
    assert_eq!(copied.bytes, bytes);

    println!(
        "\nspeedup vs legacy: move {:.0}x, copy-fallback {:.1}x",
        legacy_elapsed.as_secs_f64() / move_elapsed.as_secs_f64().max(1e-9),
        legacy_elapsed.as_secs_f64() / copy_elapsed.as_secs_f64().max(1e-9),
    );

    let _ = fs::remove_dir_all(&base);
}

/// End-to-end through the real Claude archiver, on a throwaway home.
#[test]
#[ignore = "timing benchmark; writes ~500 MB to the temp volume"]
fn claude_create_and_restore_profile_timing() {
    use crate::services::claude_archiver;

    let home = std::env::temp_dir().join(format!("dejavu-profile-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let paths = ClaudePaths::for_home(&home);
    fs::create_dir_all(&paths.claude_dir).expect("claude dir");
    fs::write(&paths.claude_md, "# global").expect("claude md");
    let (bytes, files) = seed_projects(&paths.projects_dir, 60, 8);
    println!(
        "\nseeded {} files / {:.0} MB under {}",
        files,
        bytes as f64 / 1024.0 / 1024.0,
        paths.projects_dir.display()
    );

    let archive = timed("create_profile", || {
        claude_archiver::create_profile(&paths, Some("bench".to_string())).expect("create")
    });
    println!(
        "  archived {} item(s), {} ({} bytes)",
        archive.items, archive.size_human, archive.total_size
    );
    assert_eq!(archive.total_size, bytes + 8);
    assert!(!paths.projects_dir.exists(), "live projects should be gone");

    timed("restore_profile", || {
        claude_archiver::restore_profile(&paths, &archive.name).expect("restore")
    });
    assert!(
        paths
            .projects_dir
            .join("C--Codes-project-000")
            .join("session-000.jsonl")
            .exists(),
        "restore did not put the sessions back"
    );
    // The archive must survive being restored from, and the pre-restore state must be recoverable.
    let profiles = claude_archiver::list_profiles(&paths).expect("list");
    println!(
        "  snapshots now: {:?}",
        profiles.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    assert!(profiles.iter().any(|p| p.name == archive.name));

    timed("delete_profile", || {
        claude_archiver::delete_profile(&paths, &archive.name).expect("delete")
    });

    let _: PathBuf = home.clone();
    let _ = fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// Live OpenCode snapshot round-trip
// ---------------------------------------------------------------------------

/// Snapshot the **real** OpenCode install on this machine and restore it again.
///
/// Destructive by design: creating a snapshot moves the live data into the archive, exactly as the
/// Snapshots page does. The test restores it and is only meaningful together with the manifest
/// comparison the caller runs before and after, so it prints every intermediate state rather than
/// asserting quietly.
#[test]
#[ignore = "moves this machine's real OpenCode data into an archive and back"]
fn opencode_live_snapshot_round_trip() {
    use crate::agents::OpenCodeProvider;

    // `--ignored` runs every ignored test at once, and this one relocates real user data. Require
    // an explicit opt-in so it can only be triggered on purpose:
    //   DEJAVU_LIVE_SNAPSHOT=1 cargo test --lib smoke::opencode_live -- --ignored --nocapture
    if std::env::var("DEJAVU_LIVE_SNAPSHOT").as_deref() != Ok("1") {
        println!("skipped: set DEJAVU_LIVE_SNAPSHOT=1 to run this against real data");
        return;
    }

    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .expect("home");
    let config_dir = home.join(".config").join("opencode");
    let data_dir = home.join(".local").join("share").join("opencode");
    let preserved = ["auth.json", "account.json"];

    let before = (fsops_stats(&config_dir), fsops_stats(&data_dir));
    println!(
        "before: config {} files/{:.1} MB · data {} files/{:.1} MB",
        before.0 .1,
        before.0 .0 as f64 / 1024.0 / 1024.0,
        before.1 .1,
        before.1 .0 as f64 / 1024.0 / 1024.0
    );

    let provider = OpenCodeProvider::new();
    assert!(
        provider.available(),
        "OpenCode has no local data to snapshot"
    );

    let archive = timed("create_profile", || {
        provider
            .create_profile(Some("smoke-roundtrip".to_string()))
            .expect("create snapshot")
    });
    println!(
        "  snapshot {} · {} item(s) · {} ({} bytes)",
        archive.name, archive.items, archive.size_human, archive.total_size
    );

    // The live directories must now hold nothing but the preserved credentials.
    let after_create = (fsops_stats(&config_dir), fsops_stats(&data_dir));
    println!(
        "after create: config {} files · data {} files",
        after_create.0 .1, after_create.1 .1
    );
    for name in preserved {
        let path = data_dir.join(name);
        println!("  preserved {} -> {}", name, path.exists());
    }
    assert_eq!(after_create.0 .1, 0, "config dir should be empty");
    assert!(
        after_create.1 .1 <= preserved.len() as u64,
        "data dir should hold only preserved credentials, found {} files",
        after_create.1 .1
    );

    let listed = provider.list_profiles().expect("list");
    println!(
        "  listed snapshots: {:?}",
        listed.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    assert!(listed.iter().any(|p| p.name == archive.name));

    timed("restore_profile", || {
        provider.restore_profile(&archive.name).expect("restore")
    });
    let after_restore = (fsops_stats(&config_dir), fsops_stats(&data_dir));
    println!(
        "after restore: config {} files/{:.1} MB · data {} files/{:.1} MB",
        after_restore.0 .1,
        after_restore.0 .0 as f64 / 1024.0 / 1024.0,
        after_restore.1 .1,
        after_restore.1 .0 as f64 / 1024.0 / 1024.0
    );

    // Clean up so the machine is left exactly as it was found.
    for profile in provider.list_profiles().expect("list") {
        if profile.name == archive.name || profile.is_auto {
            timed(&format!("delete_profile {}", profile.name), || {
                provider.delete_profile(&profile.name).expect("delete")
            });
        }
    }
    println!(
        "remaining snapshots: {:?}",
        provider
            .list_profiles()
            .expect("list")
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
    );

    assert_eq!(
        after_restore.0, before.0,
        "config dir differs after the round trip"
    );
    assert_eq!(
        after_restore.1, before.1,
        "data dir differs after the round trip"
    );
}

fn fsops_stats(path: &Path) -> (u64, u64) {
    let t = crate::services::fsops::dir_stats(path, &[]);
    (t.bytes, t.files)
}
