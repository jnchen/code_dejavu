/**
 * Copy real agent data into a disposable home for read-only core-capability E2E.
 *
 * The originals are never modified. The OpenCode SQLite database is copied through SQLite's
 * online backup API so an active WAL cannot produce a torn copy. Secrets are not copied.
 */
import { cpSync, existsSync, mkdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join, resolve } from "node:path";

const repo = resolve(new URL("../..", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1"));
const dest = process.argv[2] ?? join(repo, ".tmp-real-e2e", "fixtures");
const home = join(dest, "home");
const appdata = join(dest, "appdata");
const localappdata = join(dest, "localappdata");
const userHome = process.env.USERPROFILE ?? process.env.HOME ?? "";

const SECRET_NAMES = new Set([
  "auth.json",
  "account.json",
  ".credentials.json",
  "credentials.json",
]);

function copy(src, dst) {
  if (!existsSync(src)) return false;
  cpSync(src, dst, {
    recursive: true,
    force: true,
    filter: (path) => !SECRET_NAMES.has(path.split(/[\\/]/).pop() ?? ""),
  });
  return true;
}

function dirMtime(path) {
  try {
    return statSync(path).mtimeMs;
  } catch {
    return null;
  }
}

rmSync(dest, { recursive: true, force: true });
mkdirSync(home, { recursive: true });
mkdirSync(appdata, { recursive: true });
mkdirSync(join(appdata, "CodeDejavu"), { recursive: true });
mkdirSync(localappdata, { recursive: true });

const originals = {
  claudeProjects: join(userHome, ".claude", "projects"),
  codexSessions: join(userHome, ".codex", "sessions"),
  piSessions: join(userHome, ".pi", "agent", "sessions"),
  dshSessions: join(userHome, ".dsh", "sessions"),
  opencodeDb: join(userHome, ".local", "share", "opencode", "opencode.db"),
};

const stayed = Object.fromEntries(
  Object.entries(originals).map(([key, path]) => [key, dirMtime(path)]),
);

copy(join(userHome, ".claude", "projects"), join(home, ".claude", "projects"));
copy(join(userHome, ".claude", "CLAUDE.md"), join(home, ".claude", "CLAUDE.md"));
copy(join(userHome, ".claude", "settings.json"), join(home, ".claude", "settings.json"));
copy(join(userHome, ".claude", "rules"), join(home, ".claude", "rules"));
copy(join(userHome, ".claude", "skills"), join(home, ".claude", "skills"));
copy(join(userHome, ".claude", "commands"), join(home, ".claude", "commands"));

copy(join(userHome, ".codex", "sessions"), join(home, ".codex", "sessions"));
copy(join(userHome, ".codex", "config.toml"), join(home, ".codex", "config.toml"));

copy(join(userHome, ".pi", "agent", "sessions"), join(home, ".pi", "agent", "sessions"));
copy(join(userHome, ".pi", "agent", "settings.json"), join(home, ".pi", "agent", "settings.json"));

copy(join(userHome, ".dsh", "sessions"), join(home, ".dsh", "sessions"));
copy(join(userHome, ".dsh", "AGENTS.md"), join(home, ".dsh", "AGENTS.md"));

copy(join(userHome, ".config", "opencode"), join(home, ".config", "opencode"));

// SQLite online backup: consistent even while OpenCode has the DB open with a WAL.
const opencodeOut = join(home, ".local", "share", "opencode", "opencode.db");
mkdirSync(join(opencodeOut, ".."), { recursive: true });
if (existsSync(originals.opencodeDb)) {
  const script = [
    "import sqlite3,sys",
    "src,dst=sys.argv[1],sys.argv[2]",
    "s=sqlite3.connect(f'file:{src}?mode=ro',uri=True)",
    "d=sqlite3.connect(dst)",
    "s.backup(d)",
    "d.close();s.close()",
  ].join("\n");
  const result = spawnSync("python", ["-c", script, originals.opencodeDb, opencodeOut], {
    encoding: "utf-8",
  });
  if (result.status !== 0) throw new Error(`OpenCode backup failed: ${result.stderr}`);
}

// WSL discovery is off: this test must not boot or read any real distro.
writeFileSync(
  join(appdata, "CodeDejavu", "config.json"),
  JSON.stringify(
    {
      shell: "pwsh",
      env: {},
      agent_args: {},
      prices: [],
      wsl_scan: false,
      wsl_excluded: [],
      legacy_claude_args: [],
    },
    null,
    2,
  ),
);

// Pick a token from the newest Claude transcript for a real in-session search assertion.
import { readdirSync, readFileSync } from "node:fs";
function newestClaudeFile() {
  const root = join(home, ".claude", "projects");
  let best = null;
  for (const project of readdirSync(root, { withFileTypes: true })) {
    if (!project.isDirectory()) continue;
    for (const file of readdirSync(join(root, project.name), { withFileTypes: true })) {
      if (!file.isFile() || !file.name.endsWith(".jsonl")) continue;
      const path = join(root, project.name, file.name);
      const mtime = statSync(path).mtimeMs;
      if (!best || mtime > best.mtime) best = { path, mtime };
    }
  }
  return best?.path ?? null;
}

let sampleToken = "the";
const newest = newestClaudeFile();
if (newest) {
  const lines = readFileSync(newest, "utf8").split("\n");
  const ignore = new Set([
    "type",
    "message",
    "assistant",
    "content",
    "timestamp",
    "sessionId",
    "parentUuid",
    "isSidechain",
    "queue-operation",
    "attachment",
    "system-reminder",
  ]);
  outer: for (const line of lines) {
    if (!line.trim()) continue;
    let value;
    try {
      value = JSON.parse(line);
    } catch {
      continue;
    }
    const content = value?.message?.content;
    const text =
      typeof content === "string"
        ? content
        : Array.isArray(content)
          ? content
              .map((part) => (typeof part?.text === "string" ? part.text : ""))
              .join("\n")
          : "";
    const match = text.match(/\b[A-Za-z][A-Za-z0-9_-]{3,15}\b/g);
    for (const word of match ?? []) {
      if (!ignore.has(word.toLowerCase())) {
        sampleToken = word;
        break outer;
      }
    }
  }
}

const manifest = {
  home,
  appdata,
  localappdata,
  sampleToken,
  originals: stayed,
  counts: {
    claude: existsSync(originals.claudeProjects)
      ? readdirSync(originals.claudeProjects, { withFileTypes: true }).length
      : 0,
  },
  hasDsh: existsSync(originals.dshSessions),
};
writeFileSync(join(dest, "manifest.json"), JSON.stringify(manifest, null, 2));
console.log(JSON.stringify(manifest, null, 2));
