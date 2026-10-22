import type { Page } from "@playwright/test";
import type {
  Capabilities,
  DashboardSummary,
  IndexStatus,
  PaginatedRecords,
  SessionRecord,
  SessionSummary,
  SourceInfo,
  SubagentInfo,
} from "../../src/lib/types";

const capabilities: Capabilities = {
  sessions_read: true,
  sessions_search: true,
  sessions_resume: true,
  sessions_subagents: true,
  rules_read: true,
  rules_write: true,
  memory_read: true,
  memory_write: true,
  instructions_read: true,
  instructions_write: true,
  archive_read: true,
  archive_write: true,
  config_format: "json",
};

const sources: SourceInfo[] = [
  { id: "claude", display_name: "Claude Code", available: true, capabilities },
  { id: "codex", display_name: "Codex", available: true, capabilities },
  { id: "opencode", display_name: "OpenCode", available: true, capabilities },
  { id: "pi", display_name: "PiAgent", available: true, capabilities },
];

const session: SessionSummary = {
  source: "claude",
  session_id: "11111111-2222-3333-4444-555555555555",
  project: "C--Codes-demo",
  project_path: "C:\\Codes\\demo",
  first_prompt: "修复登录流程，定位失败原因。",
  agent_title: "修复登录流程",
  created_at: "2026-10-20 10:00:00",
  updated_at: "2026-10-21 12:00:00",
  timestamp: "2026-10-21 12:00:00",
  file_size_bytes: 2048,
  subagent_count: 1,
  model_contexts: [{ provider: "anthropic", model: "claude-sonnet-4-5" }],
};

const subagent: SubagentInfo = {
  agent_id: "agent-demo-1",
  agent_type: "general-purpose",
  description: "检查登录模块",
  tool_use_id: "call-agent-1",
  record_count: 3,
};

const records: SessionRecord[] = [
  {
    record_type: "user",
    content_preview: "请检查登录流程，定位失败原因。",
    timestamp: "2026-10-21 12:00:01",
    tool_name: null,
    tool_use_id: null,
    tool_input: null,
    diff: null,
    level: "content",
    byte_offset: 0,
  },
  {
    record_type: "assistant",
    content_preview: "我先让子代理检查登录模块，再汇总结果。",
    timestamp: "2026-10-21 12:00:02",
    tool_name: null,
    tool_use_id: null,
    tool_input: null,
    diff: null,
    level: "content",
    byte_offset: 1,
  },
  {
    record_type: "assistant",
    content_preview: "",
    timestamp: "2026-10-21 12:00:03",
    tool_name: "Agent",
    tool_use_id: "call-agent-1",
    tool_input: { prompt: "检查登录模块的 token 过期判断", description: "检查登录模块" },
    diff: null,
    level: "tool",
    byte_offset: 2,
    group_id: "turn-1",
  },
  {
    record_type: "tool_result",
    content_preview: "子代理完成：auth.ts 的过期判断使用了 < 而不是 <=。",
    timestamp: "2026-10-21 12:00:04",
    tool_name: null,
    tool_use_id: "call-agent-1",
    tool_input: null,
    diff: null,
    level: "tool",
    byte_offset: 3,
  },
];

const detailPage: PaginatedRecords = {
  records,
  start_byte_offset: 0,
  next_byte_offset: 4,
  has_more: false,
  has_earlier: false,
};

const subagentPage: PaginatedRecords = {
  records: [
    {
      record_type: "assistant",
      content_preview: "子代理检查了 auth.ts，发现 token 过期判断有误。",
      timestamp: "2026-10-21 12:00:03",
      tool_name: null,
      tool_use_id: null,
      tool_input: null,
      diff: null,
      level: "content",
      byte_offset: 0,
    },
  ],
  start_byte_offset: 0,
  next_byte_offset: 1,
  has_more: false,
  has_earlier: false,
};

const dashboard: DashboardSummary = {
  total_sessions: 3,
  recent: [session],
  by_source: [
    { source: "claude", count: 2, last_active: "2026-10-21 12:00:00" },
    { source: "codex", count: 1, last_active: "2026-10-20 09:00:00" },
    { source: "opencode", count: 0, last_active: null },
    { source: "pi", count: 0, last_active: null },
  ],
  activity: [
    { day: "2026-10-20", count: 1 },
    { day: "2026-10-21", count: 2 },
  ],
  top_projects: [{ path: "C:\\Codes\\demo", count: 3, last_active: "2026-10-21 12:00:00" }],
};

const indexStatus: IndexStatus = {
  Ready: { session_count: 3, token_count: 12_345, failed_files: 0 },
};

/**
 * Install a Tauri IPC double before the app boots. Every command is answered from in-memory
 * fixtures; no filesystem command exists, so the real ~/.claude, ~/.codex, OpenCode DB and Pi
 * sessions cannot be reached by these tests.
 */
export async function installMockTauri(page: Page): Promise<void> {
  await page.addInitScript(
    (data: unknown) => {
      const fixtures = data as {
        sources: unknown;
        session: unknown;
        subagent: unknown;
        detail: unknown;
        subagentPage: unknown;
        dashboard: unknown;
        indexStatus: unknown;
      };
      const calls: string[] = [];
      (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__ = calls;

      const profile = {
        source: "claude",
        source_display_name: "Claude Code",
        name: "snap-e2e",
        created: "2026-10-22 10:00",
        items: 3,
        total_size: 1024,
        size_human: "1.0 KB",
        note: null,
        is_auto: false,
      };
      const instruction = {
        source: "claude",
        source_display_name: "Claude Code",
        title: "全局 CLAUDE.md",
        scope: "global",
        kind: "instructions",
        path: "C:\\mock\\CLAUDE.md",
        exists: true,
        editable: true,
        size_bytes: 18,
        description: "Mock instruction file",
      };
      let profiles: unknown[] = [];
      let rules: Array<Record<string, unknown>> = [
        {
          source: "claude",
          source_display_name: "Claude Code",
          scope: "global",
          category: "root",
          filename: "mock-rule.md",
          path: "C:\\mock\\rules\\mock-rule.md",
          content: "# Mock rule",
          size_bytes: 11,
          enabled: true,
          toggleable: true,
          frontmatter: null,
        },
      ];
      let memories: Array<Record<string, unknown>> = [
        {
          source: "claude",
          source_display_name: "Claude Code",
          project: "C--Codes-demo",
          project_path: "C:\\Codes\\demo",
          filename: "note.md",
          frontmatter: {
            name: "Mock memory",
            description: "Remember this",
            type: "project",
            metadata: null,
          },
          content: "# Mock memory\n\nbody",
          size_bytes: 20,
        },
      ];
      const project = {
        source: "claude",
        source_display_name: "Claude Code",
        slug: "C--Codes-demo",
        display_path: "C:\\Codes\\demo",
        memory_count: 1,
        session_count: 1,
        last_active: "2026-10-22 10:00",
      };

      const handlers: Record<string, (args: Record<string, unknown>) => unknown> = {
        list_profiles: () => profiles,
        create_profile: () => {
          profiles = [profile];
          return profile;
        },
        restore_profile: () => null,
        delete_profile: (args) => {
          profiles = profiles.filter((item) => (item as { name?: string }).name !== args.name);
          return null;
        },
        rename_profile: () => null,
        list_rules: () => rules,
        toggle_rule: (args) => {
          rules = rules.map((rule) =>
            rule.filename === args.filename ? { ...rule, enabled: args.enabled } : rule,
          );
          return null;
        },
        list_projects: () => [project],
        list_memories: () => memories,
        get_memory: () => memories[0],
        save_memory: (args) => {
          memories = memories.map((memory) =>
            memory.filename === args.filename ? { ...memory, content: args.content } : memory,
          );
          return null;
        },
        delete_memory: (args) => {
          memories = memories.filter((memory) => memory.filename !== args.filename);
          return null;
        },
        create_memory: () => null,
        list_instruction_artifacts: () => [instruction],
        get_instruction_artifact: () => ({ ...instruction, content: "# Mock instructions" }),
        save_instruction_artifact: () => null,
        save_dejavu_config: () => null,
      };

      const defaults: Record<string, unknown> = {
        list_sources: fixtures.sources,
        list_sessions: [fixtures.session],
        browse_sessions: [fixtures.session],
        search_sessions: [fixtures.session],
        deep_search: [fixtures.session],
        dashboard_summary: fixtures.dashboard,
        get_index_status: fixtures.indexStatus,
        rebuild_index: fixtures.indexStatus,
        list_session_meta: {},
        get_session_first_prompt: "修复登录流程，定位失败原因。",
        list_subagents: [fixtures.subagent],
        get_session_detail: fixtures.detail,
        get_session_before: fixtures.detail,
        get_session_tail: fixtures.detail,
        get_subagent_detail: fixtures.subagentPage,
        search_in_session: [
          {
            byte_offset: 2,
            snippet: "子代理完成：auth.ts 的过期判断使用了 < 而不是 <=。",
            record_type: "tool_result",
            timestamp: "2026-10-21 12:00:04",
          },
        ],
        list_session_processes: [],
        list_profiles: [],
        list_workflows: [],
        list_instruction_artifacts: [],
        list_rules: [],
        list_projects: [],
        list_memories: [],
        usage_summary: {
          totals: {
            sessions: 3,
            input_tokens: 1000,
            output_tokens: 500,
            cache_tokens: 0,
            total_tokens: 1500,
          },
          by_source: [],
          by_model: [],
          by_project: [],
          by_day: [],
        },
        get_dejavu_config: {
          shell: "",
          env: {},
          agent_args: {},
          prices: [],
          wsl_scan: false,
          wsl_excluded: [],
        },
        list_tools: { sources: [] },
        refresh_model_prices: { models: [], source: "none", used_fallback: true, error: null },
        get_project_context: {
          source: "claude",
          project: "C--Codes-demo",
          project_path: "C:\\Codes\\demo",
          instructions: [],
          rules: [],
          memories: [],
        },
      };

      (window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {
        invoke: async (cmd: string, args: Record<string, unknown> = {}) => {
          calls.push(cmd);
          if (cmd in handlers) return handlers[cmd](args);
          if (cmd in defaults) return defaults[cmd];
          // Mutations are accepted but do nothing outside this process.
          return null;
        },
        transformCallback: (callback: unknown) => callback,
        convertFileSrc: (path: string) => path,
      };
      (window as unknown as { __TAURI_EVENT_PLUGIN_INTERNALS__: unknown })
        .__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
    },
    { sources, session, subagent, detail: detailPage, subagentPage, dashboard, indexStatus },
  );
}
