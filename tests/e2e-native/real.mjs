/**
 * Read-only core-capability E2E against a COPY of real agent data.
 *
 * Prepare once with `node tests/e2e-native/real-prepare.mjs`, then run this script. The app's
 * USERPROFILE/APPDATA/LOCALAPPDATA/XDG paths all point into `.tmp-real-e2e/fixtures`, so the
 * originals are never opened. Nothing destructive is exercised.
 */
import { spawn } from "node:child_process";
import { existsSync, readFileSync, rmSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const fixtureRoot = join(repo, ".tmp-real-e2e", "fixtures");
const manifestPath = join(fixtureRoot, "manifest.json");
const port = 4445;

if (!existsSync(manifestPath)) {
  throw new Error("Run `node tests/e2e-native/real-prepare.mjs` first");
}
const fixtures = JSON.parse(readFileSync(manifestPath, "utf8"));
if (!resolve(fixtures.home).startsWith(resolve(fixtureRoot))) {
  throw new Error("Fixture home escaped .tmp-real-e2e; refusing to start");
}

const appName = process.platform === "win32" ? "code-dejavu.exe" : "code-dejavu";
const app = join(repo, "src-tauri", "target", "debug", appName);
if (!existsSync(app)) throw new Error(`Build the app first: ${app}`);

const env = {
  ...process.env,
  USERPROFILE: fixtures.home,
  HOME: fixtures.home,
  APPDATA: fixtures.appdata,
  LOCALAPPDATA: fixtures.localappdata,
  XDG_DATA_HOME: join(fixtureRoot, "xdg-data"),
  XDG_CONFIG_HOME: join(fixtureRoot, "xdg-config"),
  XDG_CACHE_HOME: join(fixtureRoot, "xdg-cache"),
  XDG_STATE_HOME: join(fixtureRoot, "xdg-state"),
  TAURI_WEBDRIVER_PORT: String(port),
};

const child = spawn(app, [], { env, stdio: ["ignore", "pipe", "pipe"] });
let appLog = "";
child.stdout.on("data", (chunk) => (appLog += chunk.toString()));
child.stderr.on("data", (chunk) => (appLog += chunk.toString()));

const base = `http://127.0.0.1:${port}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitForDriver() {
  for (let i = 0; i < 300; i++) {
    try {
      const res = await fetch(`${base}/status`);
      if (res.ok) return;
    } catch {}
    await sleep(200);
  }
  throw new Error(`embedded WebDriver did not start\n${appLog}`);
}

let sessionId = null;
async function wd(method, path, body) {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: body ? { "content-type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) {
    const error = new Error(`${method} ${path}: ${json.value?.error ?? res.status} ${json.value?.message ?? ""}`);
    error.status = res.status;
    throw error;
  }
  return json.value;
}

async function find(xpath) {
  try {
    const value = await wd("POST", `/session/${sessionId}/element`, {
      using: "xpath",
      value: xpath,
    });
    return value["element-6066-11e4-a52e-4f735466cecf"];
  } catch (error) {
    if (error.status === 404) return null;
    throw error;
  }
}

async function waitFor(xpath, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const element = await find(xpath);
    if (element) return element;
    await sleep(250);
  }
  throw new Error(`Timed out waiting for ${xpath}\n${appLog}`);
}

async function click(xpath) {
  const element = await waitFor(xpath);
  await wd("POST", `/session/${sessionId}/element/${element}/click`);
}

async function type(xpath, value) {
  const element = await waitFor(xpath);
  await wd("POST", `/session/${sessionId}/element/${element}/value`, {
    text: value,
    value: [...value],
  });
}

async function bodyText() {
  const result = await wd("POST", `/session/${sessionId}/execute/sync`, {
    script: "return document.body.innerText",
    args: [],
  });
  return String(result ?? "");
}

async function waitForBody(predicate, timeout = 120_000, label = "body condition") {
  const deadline = Date.now() + timeout;
  let text = "";
  while (Date.now() < deadline) {
    text = await bodyText().catch(() => "");
    if (predicate(text)) return text;
    await sleep(500);
  }
  throw new Error(`Timed out waiting for ${label}\n${text.slice(0, 4000)}\n${appLog}`);
}

function sourceCount(text, name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = text.match(new RegExp(`${escaped}[\\s\\S]{0,80}?(\\d+)\\s*会话`));
  return match ? Number(match[1]) : 0;
}

async function main() {
  await waitForDriver();
  const created = await wd("POST", "/session", {
    capabilities: { alwaysMatch: { browserName: "tauri" } },
  });
  sessionId = created.sessionId;

  await waitFor('//h1[contains(., "控制台")]');
  await waitForBody((text) => /已索引\s+\d+\s+个会话/.test(text), 300_000, "index ready");
  const dashboard = await bodyText();
  const indexed = Number(dashboard.match(/已索引\s+(\d+)\s+个会话/)?.[1] ?? 0);
  if (indexed < 50) throw new Error(`Unexpectedly low real index count: ${indexed}`);
  console.log(`ok real index built: ${indexed} sessions`);

  await click("//a[contains(@href, '/sessions')]");
  await waitForBody((text) => text.includes("会话历史"));

  const sources = ["Claude Code", "Codex CLI", "OpenCode", "PiAgent"];
  if (fixtures.hasDsh) sources.push("DeepSeek Harness");
  for (const name of sources) {
    await click(`//button[normalize-space(.)='${name}']`);
    const text = await waitForBody(
      (value) => /共\s+\d+\s+个会话/.test(value),
      120_000,
      `${name} session list`,
    );
    const count = Number(text.match(/共\s+(\d+)\s+个会话/)?.[1] ?? 0);
    if (count <= 0) throw new Error(`${name} had no sessions in the real copy`);
    console.log(`ok ${name}: ${count} sessions`);
  }

  // Open the newest Claude session and search inside it using a token from the real transcript.
  await click("//button[normalize-space(.)='Claude Code']");
  await waitForBody((text) => /共\s+\d+\s+个会话/.test(text));
  await click("//button[contains(@class,'w-full') and contains(@class,'text-left')][1]");
  await waitFor("//button[normalize-space(.)='← 返回']");
  await waitForBody((text) => text.length > 0);
  await type("//input[@placeholder='在此会话中搜索...']", fixtures.sampleToken);
  await waitForBody(
    (text) => /\d+\/\d+/.test(text),
    60_000,
    `in-session search for ${fixtures.sampleToken}`,
  );
  console.log(`ok real in-session search: "${fixtures.sampleToken}"`);
  await click("//button[normalize-space(.)='← 返回']");

  // Global search across the real index.
  await click("//button[normalize-space(.)='全部']");
  await type("//input[contains(@placeholder,'搜索')]", fixtures.sampleToken);
  const searchText = await waitForBody(
    (text) => /找到\s+\d+\s+条|共\s+\d+\s+个会话/.test(text),
    60_000,
    "global search",
  );
  if (/找到\s+0\s+条/.test(searchText)) throw new Error("Global search returned no hits");
  console.log("ok real global search");

  const changed = [];
  for (const [key, before] of Object.entries(fixtures.originals ?? {})) {
    if (before == null) continue;
    try {
      const realPath = {
        claudeProjects: join(process.env.USERPROFILE, ".claude", "projects"),
        codexSessions: join(process.env.USERPROFILE, ".codex", "sessions"),
        piSessions: join(process.env.USERPROFILE, ".pi", "agent", "sessions"),
        dshSessions: join(process.env.USERPROFILE, ".dsh", "sessions"),
        opencodeDb: join(process.env.USERPROFILE, ".local", "share", "opencode", "opencode.db"),
      }[key];
      const after = statSync(realPath).mtimeMs;
      if (after !== before) changed.push(key);
    } catch {}
  }
  if (changed.length) {
    console.warn(`warning: original paths changed while the test ran (other agents may be active): ${changed.join(", ")}`);
  } else {
    console.log("ok original data mtimes unchanged");
  }
}

try {
  await main();
  console.log("REAL-DATA CORE E2E PASSED");
} finally {
  if (sessionId) await wd("DELETE", `/session/${sessionId}`).catch(() => {});
  child.kill();
  if (!process.env.E2E_KEEP) rmSync(fixtureRoot, { recursive: true, force: true });
}
