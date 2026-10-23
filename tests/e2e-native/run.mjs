/**
 * Native Tauri E2E smoke test using the embedded WebDriver server.
 *
 * Before running:
 *   1. pnpm build
 *   2. cargo build --manifest-path src-tauri/Cargo.toml --features e2e
 *
 * The app is launched with USERPROFILE/APPDATA/LOCALAPPDATA pointing at `.tmp-e2e/fixtures`, so
 * real agent data is never read. `seed.mjs` writes the Claude/OpenCode fixtures into that home.
 * The `e2e` Cargo feature embeds `tauri-plugin-wdio-webdriver` and is never enabled in releases.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, rmdirSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { seed } from "./seed.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const root = join(repo, ".tmp-e2e");
const fixtureRoot = join(root, "fixtures");
const driverPort = 4445;
const appName = process.platform === "win32" ? "code-dejavu.exe" : "code-dejavu";
const app = join(repo, "src-tauri", "target", "debug", appName);

if (!existsSync(app)) throw new Error(`Build the app first: ${app}`);

const fixtures = seed(fixtureRoot);
mkdirSync(join(fixtureRoot, "xdg-data"), { recursive: true });
mkdirSync(join(fixtureRoot, "xdg-config"), { recursive: true });
mkdirSync(join(fixtureRoot, "xdg-cache"), { recursive: true });
mkdirSync(join(fixtureRoot, "xdg-state"), { recursive: true });

const appEnv = {
  ...process.env,
  USERPROFILE: fixtures.home,
  HOME: fixtures.home,
  APPDATA: fixtures.appdata,
  LOCALAPPDATA: fixtures.localappdata,
  XDG_DATA_HOME: join(fixtureRoot, "xdg-data"),
  XDG_CONFIG_HOME: join(fixtureRoot, "xdg-config"),
  XDG_CACHE_HOME: join(fixtureRoot, "xdg-cache"),
  XDG_STATE_HOME: join(fixtureRoot, "xdg-state"),
  TAURI_WEBDRIVER_PORT: String(driverPort),
};

const child = spawn(app, [], { env: appEnv, stdio: ["ignore", "pipe", "pipe"] });
let appLog = "";
child.stdout.on("data", (chunk) => (appLog += chunk.toString()));
child.stderr.on("data", (chunk) => (appLog += chunk.toString()));

const base = `http://127.0.0.1:${driverPort}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitForDriver() {
  for (let i = 0; i < 150; i++) {
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

async function waitFor(xpath, timeout = 30_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const element = await find(xpath);
    if (element) return element;
    await sleep(200);
  }
  const source = await wd("GET", `/session/${sessionId}/source`).catch(() => "");
  throw new Error(
    `Timed out waiting for ${xpath}\n\nPage source:\n${String(source).slice(0, 4000)}\n\nApp log:\n${appLog}`,
  );
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

function leafText(value) {
  return `//*[contains(text(), ${JSON.stringify(value)})]`;
}

function buttonText(value) {
  return `//button[contains(normalize-space(.), ${JSON.stringify(value)})]`;
}

async function main() {
  await waitForDriver();
  const created = await wd("POST", "/session", {
    capabilities: { alwaysMatch: { browserName: "tauri" } },
  });
  sessionId = created.sessionId;
  if (!sessionId) throw new Error(`No WebDriver session: ${JSON.stringify(created)}`);

  await waitFor('//h1[contains(., "控制台")]');
  console.log("ok dashboard is visible");

  await click("//a[contains(@href, '/sessions')]");
  await waitFor(leafText("会话历史"));
  await waitFor(buttonText("Claude 隔离会话"));
  console.log("ok Claude session listed");

  await click(buttonText("Claude 隔离会话"));
  await waitFor(leafText("这是来自隔离夹具的 Claude 回复"));
  await click("//button[normalize-space(.)='+工具']");
  await click(buttonText("根子代理"));
  await waitFor(leafText("根子代理开始"));
  await click(buttonText("explore"));
  await waitFor(leafText("孙代理完成：嵌套检查通过。"));
  console.log("ok two-level Claude subagents expand");
  await click("//button[normalize-space(.)='← 返回']");

  await click(`//button[normalize-space(.)='OpenCode']`);
  await waitFor(buttonText("OpenCode v2 隔离会话"));
  await waitFor(buttonText("OpenCode v1 隔离会话"));
  console.log("ok OpenCode v2 and v1-only sessions coexist");

  await click(buttonText("OpenCode v2 隔离会话"));
  await waitFor(leafText("v2 消息进来"));
  await waitFor(leafText("v2 助手回复"));
  console.log("ok v2 session detail rendered");

  await click("//button[normalize-space(.)='+工具']");
  await waitFor(leafText("读取文件"));
  console.log("ok v2 tool call rendered");

  await type("//input[@placeholder='在此会话中搜索...']", "助手");
  await waitFor("//button[normalize-space(.)='1/1']");
  console.log("ok in-session search uses v2 messages");

  await click("//button[normalize-space(.)='← 返回']");
  await click("//button[normalize-space(.)='Codex CLI']");
  await waitFor(buttonText("Codex 隔离会话"));
  await click(buttonText("Codex 隔离会话"));
  await waitFor(leafText("Codex 助手回复"));
  console.log("ok Codex rollout parsed");

  await click("//button[normalize-space(.)='← 返回']");
  await click("//button[normalize-space(.)='PiAgent']");
  await waitFor(buttonText("Pi 隔离会话"));
  await click(buttonText("Pi 隔离会话"));
  await waitFor(leafText("Pi 助手回复"));
  console.log("ok Pi session tree parsed");
}

try {
  await main();
  console.log("NATIVE E2E PASSED");
} finally {
  if (sessionId) await wd("DELETE", `/session/${sessionId}`).catch(() => {});
  child.kill();
  if (!process.env.E2E_KEEP) {
    rmSync(fixtureRoot, { recursive: true, force: true });
    try {
      rmdirSync(root);
    } catch {}
  }
}
