import { invoke } from "@tauri-apps/api/core";
import type {
  ProfileArchive,
  ProjectInfo,
  MemoryFile,
  MemoryFrontmatter,
  RuleFile,
  SessionSummary,
  PaginatedRecords,
  SubagentInfo,
  SessionSearchHit,
  SessionMeta,
  WorkflowItem,
  ToolsInfo,
  UsageSummary,
  ModelPriceRefreshResult,
  DashboardSummary,
  IndexStatus,
  DejavuConfig,
  InstructionArtifact,
  InstructionDetail,
  ProjectContext,
  SourceInfo,
  SessionProcessInfo,
} from "./types";

let sourcesInFlight: Promise<SourceInfo[]> | null = null;
let sourcesCache: { value: SourceInfo[]; at: number } | null = null;

const readCache = new Map<string, { value: unknown; at: number }>();
const readInflight = new Map<string, Promise<unknown>>();
const readGeneration = new Map<string, number>();
const pendingReads: Array<() => void> = [];
let activeReads = 0;
// Backend reads run on Tauri's blocking pool, not the WebView thread. Keep a finite ceiling for
// disk-heavy pages, but allow independent route panels to load in parallel instead of queueing
// behind three stale requests after rapid navigation.
const MAX_CONCURRENT_ROUTE_READS = 8;

function scheduleRead<T>(load: () => Promise<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const start = () => {
      activeReads++;
      load().then(resolve, reject).finally(() => {
        activeReads--;
        pendingReads.shift()?.();
      });
    };
    if (activeReads < MAX_CONCURRENT_ROUTE_READS) start();
    else pendingReads.push(start);
  });
}

function cachedRead<T>(key: string, load: () => Promise<T>, ttlMs = 300, limitConcurrency = true): Promise<T> {
  const cached = readCache.get(key);
  if (cached && Date.now() - cached.at < ttlMs) return Promise.resolve(cached.value as T);
  const pending = readInflight.get(key) as Promise<T> | undefined;
  if (pending) return pending;
  const generation = readGeneration.get(key) ?? 0;
  let request: Promise<T>;
  request = (limitConcurrency ? scheduleRead(load) : load())
    .then((value) => {
      // A write may invalidate this key while the read is still running. Never let that older
      // response repopulate the cache after the mutation has completed.
      if ((readGeneration.get(key) ?? 0) === generation) {
        readCache.set(key, { value, at: Date.now() });
      }
      return value;
    })
    .finally(() => {
      if (readInflight.get(key) === request) readInflight.delete(key);
    });
  readInflight.set(key, request);
  return request;
}

function invalidateRead(key: string): void {
  readCache.delete(key);
  readInflight.delete(key);
  readGeneration.set(key, (readGeneration.get(key) ?? 0) + 1);
}

// Route changes frequently ask for the same provider inventory. Coalesce concurrent calls and
// keep a very short snapshot so rapid menu navigation cannot fan out repeated filesystem probes.
function listSources(): Promise<SourceInfo[]> {
  if (sourcesCache && Date.now() - sourcesCache.at < 500) {
    return Promise.resolve(sourcesCache.value);
  }
  if (sourcesInFlight) return sourcesInFlight;
  sourcesInFlight = invoke<SourceInfo[]>("list_sources")
    .then((value) => {
      sourcesCache = { value, at: Date.now() };
      return value;
    })
    .finally(() => {
      sourcesInFlight = null;
    });
  return sourcesInFlight;
}

export const api = {
  profiles: {
    list: (source?: string | null) => cachedRead(`profiles:${source ?? ""}`, () =>
      invoke<ProfileArchive[]>("list_profiles", { source: source ?? null })),
    create: (name?: string, source?: string | null) =>
      invoke<ProfileArchive>("create_profile", { name: name ?? null, source: source ?? null }).then((value) => {
        invalidateRead(`profiles:${source ?? ""}`);
        return value;
      }),
    restore: (name: string, source?: string | null) =>
      invoke<void>("restore_profile", { name, source: source ?? null }).then((value) => {
        invalidateRead(`profiles:${source ?? ""}`);
        return value;
      }),
    delete: (name: string, source?: string | null) =>
      invoke<void>("delete_profile", { name, source: source ?? null }).then((value) => {
        invalidateRead(`profiles:${source ?? ""}`);
        return value;
      }),
    rename: (oldName: string, newName: string, source?: string | null) =>
      invoke<void>("rename_profile", { oldName, newName, source: source ?? null }).then((value) => {
        invalidateRead(`profiles:${source ?? ""}`);
        return value;
      }),
  },
  memories: {
    listProjects: (source?: string | null) => cachedRead(`memory-projects:${source ?? ""}`, () =>
      invoke<ProjectInfo[]>("list_projects", { source: source ?? null })),
    list: (project: string, source?: string | null) => cachedRead(`memories:${source ?? ""}:${project}`, () =>
      invoke<MemoryFile[]>("list_memories", { project, source: source ?? null })),
    get: (project: string, filename: string, source?: string | null) =>
      invoke<MemoryFile>("get_memory", { project, filename, source: source ?? null }),
    save: (project: string, filename: string, frontmatterData: MemoryFrontmatter, content: string, source?: string | null) =>
      invoke<void>("save_memory", { project, filename, frontmatterData, content, source: source ?? null }).then((value) => {
        invalidateRead(`memories:${source ?? ""}:${project}`);
        invalidateRead(`memory-projects:${source ?? ""}`);
        return value;
      }),
    delete: (project: string, filename: string, source?: string | null) =>
      invoke<void>("delete_memory", { project, filename, source: source ?? null }).then((value) => {
        invalidateRead(`memories:${source ?? ""}:${project}`);
        invalidateRead(`memory-projects:${source ?? ""}`);
        return value;
      }),
    create: (project: string, filename: string, frontmatterData: MemoryFrontmatter, content: string, source?: string | null) =>
      invoke<void>("create_memory", { project, filename, frontmatterData, content, source: source ?? null }).then((value) => {
        invalidateRead(`memories:${source ?? ""}:${project}`);
        invalidateRead(`memory-projects:${source ?? ""}`);
        return value;
      }),
  },
  rules: {
    list: (source?: string | null) => cachedRead(`rules:${source ?? ""}`, () =>
      invoke<RuleFile[]>("list_rules", { source: source ?? null })),
    get: (category: string, filename: string, source?: string | null) =>
      invoke<RuleFile>("get_rule", { category, filename, source: source ?? null }),
    toggle: (category: string, filename: string, enabled: boolean, source?: string | null) =>
      invoke<void>("toggle_rule", { category, filename, enabled, source: source ?? null }).then((value) => {
        invalidateRead(`rules:${source ?? ""}`);
        invalidateRead("rules:");
        return value;
      }),
  },
  sessions: {
    /** Registered coding-agent sources (Claude, Codex, …) with their capabilities. */
    listSources,
    list: (project?: string, source?: string) =>
      invoke<SessionSummary[]>("list_sessions", { source: source ?? null, project: project ?? null }),
    /** Default (no-query) session list from the in-memory index — instant, no disk scan. */
    browse: (source?: string, archiveScope?: string) =>
      invoke<SessionSummary[]>("browse_sessions", { source: source ?? null, archiveScope: archiveScope ?? null }),
    search: (query: string, source?: string, scopes?: string[], archiveScope?: string) =>
      invoke<SessionSummary[]>("search_sessions", { source: source ?? null, query, scopes: scopes ?? null, archiveScope: archiveScope ?? null }),
    /** Exhaustive full-text search that scans session source files (slower, more complete). */
    deepSearch: (query: string, source?: string, archiveScope?: string) =>
      invoke<SessionSummary[]>("deep_search", { source: source ?? null, query, archiveScope: archiveScope ?? null }),
    getIndexStatus: () => cachedRead("index-status", () =>
      invoke<IndexStatus>("get_index_status"), 250, false),
    rebuildIndex: () => invoke<IndexStatus>("rebuild_index").then((value) => {
      invalidateRead("index-status");
      return value;
    }),
    usageSummary: () =>
      invoke<UsageSummary>("usage_summary"),
    refreshModelPrices: () =>
      invoke<ModelPriceRefreshResult>("refresh_model_prices"),
    /** Pre-aggregated dashboard view from the in-memory index (no disk scan). */
    dashboardSummary: () =>
      invoke<DashboardSummary>("dashboard_summary"),
    getDetail: (project: string, sessionId: string, byteOffset: number, limit: number, minLevel: string = "content", archiveName?: string | null, source?: string) =>
      invoke<PaginatedRecords>("get_session_detail", { source: source ?? null, project, sessionId, byteOffset, limit, minLevel, archiveName: archiveName ?? null }),
    getBefore: (project: string, sessionId: string, beforeByteOffset: number, limit: number, minLevel: string = "content", archiveName?: string | null, source?: string) =>
      invoke<PaginatedRecords>("get_session_before", { source: source ?? null, project, sessionId, beforeByteOffset, limit, minLevel, archiveName: archiveName ?? null }),
    getTail: (project: string, sessionId: string, limit: number, minLevel: string = "content", archiveName?: string | null, source?: string) =>
      invoke<PaginatedRecords>("get_session_tail", { source: source ?? null, project, sessionId, limit, minLevel, archiveName: archiveName ?? null }),
    getFirstPrompt: (project: string, sessionId: string, archiveName?: string | null, source?: string) =>
      invoke<string | null>("get_session_first_prompt", { source: source ?? null, project, sessionId, archiveName: archiveName ?? null }),
    listSubagents: (project: string, sessionId: string, archiveName?: string | null, source?: string) =>
      invoke<SubagentInfo[]>("list_subagents", { source: source ?? null, project, sessionId, archiveName: archiveName ?? null }),
    getSubagentDetail: (project: string, sessionId: string, agentId: string, byteOffset: number, limit: number, archiveName?: string | null, source?: string) =>
      invoke<PaginatedRecords>("get_subagent_detail", { source: source ?? null, project, sessionId, agentId, byteOffset, limit, archiveName: archiveName ?? null }),
    searchInSession: (project: string, sessionId: string, query: string, archiveName?: string | null, source?: string) =>
      invoke<SessionSearchHit[]>("search_in_session", { source: source ?? null, project, sessionId, query, archiveName: archiveName ?? null }),
  },
  sessionMeta: {
    /** All per-session metadata, keyed by "<source>::<session_id>". */
    list: () => cachedRead("session-meta", () => invoke<Record<string, SessionMeta>>("list_session_meta")),
    set: (key: string, meta: SessionMeta) => invoke<void>("set_session_meta", { key, meta }).then((value) => {
      invalidateRead("session-meta");
      return value;
    }),
  },
  workflows: {
    list: () => cachedRead("workflows", () => invoke<WorkflowItem[]>("list_workflows")),
    read: (source: string, path: string) => invoke<string>("read_workflow", { source, path }),
  },
  tools: {
    // Config files are small and the page has an explicit refresh button; always re-read them.
    list: () => invoke<ToolsInfo>("list_tools"),
  },
  shell: {
    resumeSession: (projectPath: string, sessionId: string, source?: string | null) =>
      invoke<void>("resume_session", { projectPath, sessionId, source: source ?? null }),
    listSessionProcesses: (projectPath: string, sessionId: string, source?: string | null) =>
      invoke<SessionProcessInfo[]>("list_session_processes", { projectPath, sessionId, source: source ?? null }),
    stopSessionProcess: (pid: number, startedAt: number, projectPath: string, sessionId: string, source?: string | null) =>
      invoke<void>("stop_session_process", { pid, startedAt, projectPath, sessionId, source: source ?? null }),
    openInTerminal: (projectPath: string) =>
      invoke<void>("open_in_terminal", { projectPath }),
    openExternal: (url: string) => invoke<void>("open_external", { url }),
    saveExport: (filename: string, content: string) =>
      invoke<string>("save_text_export", { filename, content }),
    revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  },
  dejavu: {
    getConfig: () => cachedRead("config", () => invoke<DejavuConfig>("get_dejavu_config")),
    saveConfig: (config: DejavuConfig) => invoke<void>("save_dejavu_config", { config }).then((value) => {
      invalidateRead("config");
      return value;
    }),
  },
  instructions: {
    list: () => cachedRead("instructions", () => invoke<InstructionArtifact[]>("list_instruction_artifacts")),
    get: (source: string, path: string) =>
      invoke<InstructionDetail>("get_instruction_artifact", { source, path }),
    save: (source: string, path: string, content: string) =>
      invoke<void>("save_instruction_artifact", { source, path, content }).then((value) => {
        invalidateRead("instructions");
        return value;
      }),
    projectContext: (source: string, project: string, projectPath: string) =>
      invoke<ProjectContext>("get_project_context", { source, project, projectPath }),
  },
};
