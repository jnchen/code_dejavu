/**
 * Create a disposable home for native Tauri E2E.
 *
 * Everything the app can read is placed under the returned `root`; the runner launches the app
 * with USERPROFILE/APPDATA/LOCALAPPDATA pointing here, so the real ~/.claude, ~/.codex, OpenCode
 * database and Pi sessions are never opened.
 */
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

const CLAUDE_PROJECT = "C--Codes-demo";
const CLAUDE_SESSION = "e2e-claude-0001";
const V2_SESSION = "ses_e2e_v2_0001";
const V1_SESSION = "ses_e2e_v1_0001";

function write(path, content) {
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(path, content);
}

export function seed(root) {
  rmSync(root, { recursive: true, force: true });
  const home = join(root, "home");
  const appdata = join(root, "appdata");
  const localappdata = join(root, "localappdata");
  mkdirSync(home, { recursive: true });
  mkdirSync(appdata, { recursive: true });
  mkdirSync(localappdata, { recursive: true });

  // Keep WSL discovery off so reading real distro shares cannot happen during the test.
  write(
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

  const cwd = "C:\\Codes\\demo";
  write(
    join(home, ".claude", "projects", CLAUDE_PROJECT, `${CLAUDE_SESSION}.jsonl`),
    [
      JSON.stringify({
        type: "user",
        message: { role: "user", content: [{ type: "text", text: "Claude 隔离会话" }] },
        timestamp: "2026-10-21T10:00:00.000Z",
        cwd,
        sessionId: CLAUDE_SESSION,
        uuid: "e2e-user-1",
      }),
      JSON.stringify({
        type: "assistant",
        message: {
          role: "assistant",
          content: [
            { type: "text", text: "这是来自隔离夹具的 Claude 回复" },
            {
              type: "tool_use",
              id: "call-root",
              name: "Agent",
              input: { description: "根子代理", subagent_type: "general-purpose", prompt: "检查登录" },
            },
          ],
          model: "claude-sonnet-4-5",
          usage: { input_tokens: 10, output_tokens: 5 },
        },
        timestamp: "2026-10-21T10:00:01.000Z",
        cwd,
        sessionId: CLAUDE_SESSION,
        uuid: "e2e-assistant-1",
      }),
    ].join("\n") + "\n",
  );

  // Two-level Claude subagents: the child transcript itself spawns a grandchild.
  const subagentDir = join(
    home,
    ".claude",
    "projects",
    CLAUDE_PROJECT,
    CLAUDE_SESSION,
    "subagents",
  );
  write(
    join(subagentDir, "agent-root.meta.json"),
    JSON.stringify({
      agentType: "general-purpose",
      description: "根子代理",
      toolUseId: "call-root",
      spawnDepth: 1,
    }),
  );
  write(
    join(subagentDir, "agent-root.jsonl"),
    [
      JSON.stringify({
        type: "assistant",
        isSidechain: true,
        agentId: "agent-root",
        message: {
          role: "assistant",
          content: [
            { type: "text", text: "根子代理开始" },
            {
              type: "tool_use",
              id: "call-nested",
              name: "Agent",
              input: { description: "嵌套子代理", subagent_type: "explore", prompt: "确认修复" },
            },
          ],
        },
        timestamp: "2026-10-21T10:00:02.000Z",
        sessionId: CLAUDE_SESSION,
      }),
    ].join("\n") + "\n",
  );
  write(
    join(subagentDir, "agent-nested.meta.json"),
    JSON.stringify({
      agentType: "explore",
      description: "嵌套子代理",
      toolUseId: "call-nested",
      spawnDepth: 2,
      parentAgentId: "agent-root",
    }),
  );
  write(
    join(subagentDir, "agent-nested.jsonl"),
    [
      JSON.stringify({
        type: "assistant",
        isSidechain: true,
        agentId: "agent-nested",
        message: {
          role: "assistant",
          content: [{ type: "text", text: "孙代理完成：嵌套检查通过。" }],
        },
        timestamp: "2026-10-21T10:00:03.000Z",
        sessionId: CLAUDE_SESSION,
      }),
    ].join("\n") + "\n",
  );

  const codexSession = "019f0000-0000-7000-8000-000000000001";
  write(
    join(
      home,
      ".codex",
      "sessions",
      "2026",
      "10",
      "21",
      `rollout-2026-10-21T11-00-00-${codexSession}.jsonl`,
    ),
    [
      JSON.stringify({
        timestamp: "2026-10-21T11:00:00.000Z",
        type: "session_meta",
        payload: {
          session_id: codexSession,
          id: codexSession,
          timestamp: "2026-10-21T11:00:00.000Z",
          cwd,
          originator: "codex_cli_rs",
          cli_version: "0.154.0",
          source: "cli",
          thread_source: "user",
          model_provider: "openai",
        },
      }),
      JSON.stringify({
        timestamp: "2026-10-21T11:00:01.000Z",
        type: "response_item",
        payload: {
          type: "message",
          role: "user",
          content: [{ type: "input_text", text: "Codex 隔离会话" }],
        },
      }),
      JSON.stringify({
        timestamp: "2026-10-21T11:00:02.000Z",
        type: "response_item",
        payload: {
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "Codex 助手回复" }],
        },
      }),
      JSON.stringify({
        timestamp: "2026-10-21T11:00:03.000Z",
        type: "event_msg",
        payload: {
          type: "token_count",
          info: {
            total_token_usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 },
          },
        },
      }),
    ].join("\n") + "\n",
  );

  const piSession = "018f0000-0000-7000-8000-000000000002";
  write(
    join(
      home,
      ".pi",
      "agent",
      "sessions",
      "--C--Codes-demo--",
      `2026-10-21T12-00-00-000Z_${piSession}.jsonl`,
    ),
    [
      JSON.stringify({
        type: "session",
        version: 3,
        id: piSession,
        timestamp: "2026-10-21T12:00:00.000Z",
        cwd,
      }),
      JSON.stringify({
        type: "message",
        id: "pi-user-1",
        parentId: null,
        timestamp: "2026-10-21T12:00:01.000Z",
        message: { role: "user", content: "Pi 隔离会话", timestamp: 1790000001000 },
      }),
      JSON.stringify({
        type: "message",
        id: "pi-assistant-1",
        parentId: "pi-user-1",
        timestamp: "2026-10-21T12:00:02.000Z",
        message: {
          role: "assistant",
          content: [{ type: "text", text: "Pi 助手回复" }],
          provider: "anthropic",
          model: "claude-sonnet-4-5",
          usage: { input: 10, output: 5, cacheRead: 0, cacheWrite: 0, totalTokens: 15 },
          timestamp: 1790000002000,
        },
      }),
    ].join("\n") + "\n",
  );

  // OpenCode: one v2-projected session and one v1-only session in the same database. This is the
  // exact shape an upgraded install can have, and exercises the dual-read path through real IPC.
  const dbPath = join(home, ".local", "share", "opencode", "opencode.db");
  mkdirSync(join(dbPath, ".."), { recursive: true });
  const db = new DatabaseSync(dbPath);
  db.exec(`
    CREATE TABLE session_v2 (
      id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, title TEXT,
      version TEXT, agent TEXT, model TEXT,
      tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
      tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
      tokens_cache_write INTEGER NOT NULL DEFAULT 0,
      time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL
    );
    CREATE TABLE session_message (
      id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
      time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
    );
    CREATE TABLE session (
      id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, model TEXT,
      time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL
    );
    CREATE TABLE message (
      id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL, time_created INTEGER NOT NULL
    );
    CREATE TABLE part (
      id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, data TEXT NOT NULL,
      time_created INTEGER NOT NULL
    );
  `);
  const now = Date.now();
  db.prepare(
    `INSERT INTO session_v2 (id, project_id, directory, title, version, agent, model, time_created, time_updated)
     VALUES (?, 'proj', ?, ?, '2.1.2', 'build', ?, ?, ?)`,
  ).run(
    V2_SESSION,
    cwd,
    "OpenCode v2 隔离会话",
    JSON.stringify({ id: "claude-sonnet-4-5", providerID: "anthropic" }),
    now - 2000,
    now - 1000,
  );
  const insertMessage = db.prepare(
    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
  );
  insertMessage.run(
    "msg_v2_user",
    V2_SESSION,
    "user",
    0,
    now - 2000,
    now - 2000,
    JSON.stringify({ text: "v2 消息进来", time: { created: now - 2000 } }),
  );
  insertMessage.run(
    "msg_v2_assistant",
    V2_SESSION,
    "assistant",
    1,
    now - 1000,
    now - 1000,
    JSON.stringify({
      content: [
        { type: "text", text: "v2 助手回复" },
        {
          type: "tool",
          id: "tool_v2_read",
          name: "read",
          state: {
            status: "completed",
            input: { path: "src/main.rs" },
            content: [{ type: "text", text: "fn main() {}" }],
          },
        },
      ],
      time: { created: now - 1000, completed: now - 900 },
    }),
  );

  db.prepare(
    "INSERT INTO session (id, directory, title, time_created, time_updated) VALUES (?, ?, ?, ?, ?)",
  ).run(V1_SESSION, cwd, "OpenCode v1 隔离会话", now - 3000, now - 3000);
  db.prepare(
    "INSERT INTO message (id, session_id, data, time_created) VALUES (?, ?, ?, ?)",
  ).run("msg_v1_user", V1_SESSION, JSON.stringify({ role: "user" }), now - 3000);
  db.prepare(
    "INSERT INTO part (id, message_id, session_id, data, time_created) VALUES (?, ?, ?, ?, ?)",
  ).run(
    "part_v1_text",
    "msg_v1_user",
    V1_SESSION,
    JSON.stringify({ type: "text", text: "v1 旧会话仍在" }),
    now - 3000,
  );
  db.close();

  return {
    home,
    appdata,
    localappdata,
    claudeSession: CLAUDE_SESSION,
    v2Session: V2_SESSION,
    v1Session: V1_SESSION,
  };
}

if (process.argv[1] && process.argv[1].endsWith("seed.mjs")) {
  const root = process.argv[2] ?? ".tmp-e2e/home";
  console.log(JSON.stringify(seed(root), null, 2));
}
