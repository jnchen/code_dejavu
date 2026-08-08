export interface ProfileArchive {
  source: string;
  source_display_name: string;
  name: string;
  created: string;
  items: number;
  total_size: number;
  size_human: string;
  note: string | null;
  is_auto: boolean;
}

export interface MemoryFrontmatter {
  name: string | null;
  description: string | null;
  type: string | null;
  metadata: { type?: string; tags?: string[] } | null;
}

export interface MemoryFile {
  source: string;
  source_display_name: string;
  project: string;
  project_path: string;
  filename: string;
  frontmatter: MemoryFrontmatter | null;
  content: string;
  size_bytes: number;
}

export interface ProjectInfo {
  source: string;
  source_display_name: string;
  slug: string;
  display_path: string;
  memory_count: number;
  session_count: number;
  last_active: string | null;
}

export interface RuleFile {
  source: string;
  source_display_name: string;
  scope: "global" | "project" | string;
  category: string;
  filename: string;
  path: string;
  content: string;
  size_bytes: number;
  enabled: boolean;
  toggleable: boolean;
  frontmatter: { globs?: string; always_apply?: boolean; description?: string } | null;
}

export interface Capabilities {
  sessions_read: boolean;
  sessions_search: boolean;
  sessions_resume: boolean;
  sessions_subagents: boolean;
  rules_read: boolean;
  rules_write: boolean;
  memory_read: boolean;
  memory_write: boolean;
  instructions_read: boolean;
  instructions_write: boolean;
  archive_read: boolean;
  archive_write: boolean;
  config_format: "json" | "jsonc" | "toml";
}

export interface SourceInfo {
  id: string;
  display_name: string;
  available: boolean;
  capabilities: Capabilities;
  /** Non-native machines this source also reads, e.g. ["WSL:Ubuntu"]. Empty on a plain install. */
  hosts?: string[];
}

export interface SessionModelInfo {
  provider?: string | null;
  model?: string | null;
  thinking_level?: string | null;
}

export interface SessionSummary {
  /** Which coding agent this session belongs to ("claude" | "codex" | …). */
  source: string;
  session_id: string;
  project: string;
  project_path: string;
  first_prompt: string | null;
  /** Native title / summary assigned by the coding agent. */
  agent_title?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
  /** @deprecated Backward-compatible alias for updated_at. */
  timestamp: string | null;
  file_size_bytes: number;
  subagent_count: number;
  archive_name?: string | null;
  model_contexts?: SessionModelInfo[];
}

export interface SessionSearchHit {
  byte_offset: number;
  snippet: string;
  record_type: string;
  timestamp: string | null;
}

/** App-local, per-session organisation metadata (never written to agent files). */
export interface SessionMeta {
  favorite: boolean;
  pinned: boolean;
  tags: string[];
  note: string;
}

/** A user-authored workflow artifact (skill/command/plan/task). */
export interface WorkflowItem {
  source: string;
  source_display_name: string;
  kind: string;
  name: string;
  scope: string;
  path: string;
  description: string;
  size_bytes: number;
}

export interface McpServer {
  name: string;
  scope: string;
  transport: string;
  command: string;
  args: string[];
  /** Only env var NAMES (values are intentionally never exposed). */
  env_keys: string[];
  enabled: boolean;
}

export interface HookEntry {
  event: string;
  matcher: string;
  commands: string[];
}

export interface ToolsInfo {
  sources: AgentToolsInfo[];
}

export interface AgentToolsInfo {
  source: string;
  source_display_name: string;
  available: boolean;
  mcp_servers: McpServer[];
  hooks: HookEntry[];
  mcp_source_paths: string[];
  hooks_source_paths: string[];
}

export interface UsageTotals {
  sessions: number;
  input_tokens: number;
  output_tokens: number;
  cache_tokens: number;
  total_tokens: number;
}

export interface UsageBucket {
  key: string;
  sessions: number;
  input_tokens: number;
  output_tokens: number;
  cache_tokens: number;
  total_tokens: number;
}

export interface UsageSummary {
  totals: UsageTotals;
  by_source: UsageBucket[];
  by_model: UsageBucket[];
  by_project: UsageBucket[];
  by_day: UsageBucket[];
}

export interface DashboardSourceStat {
  source: string;
  count: number;
  last_active: string | null;
}

export interface DashboardActivityDay {
  day: string;
  count: number;
}

export interface DashboardProject {
  path: string;
  count: number;
  last_active: string;
}

/** Pre-aggregated dashboard data served from the in-memory index (current sessions only). */
export interface DashboardSummary {
  total_sessions: number;
  recent: SessionSummary[];
  by_source: DashboardSourceStat[];
  activity: DashboardActivityDay[];
  top_projects: DashboardProject[];
}

export interface IndexStatus {
  Building?: null;
  Ready?: { session_count: number; token_count: number; failed_files: number };
}

export interface SessionRecord {
  record_type: string;
  content_preview: string;
  timestamp: string | null;
  tool_name: string | null;
  tool_use_id: string | null;
  tool_input: Record<string, unknown> | null;
  diff: DiffInfo | null;
  level: "content" | "tool" | "debug";
  /** Byte offset of the source line (stable per-record key for expand state). */
  byte_offset: number;
  /** Assistant message id; tool calls sharing it were a parallel batch (one model turn). */
  group_id?: string | null;
  /** Structured, tool-specific result payload parsed by the backend onto the general model. */
  result_meta?: ResultMeta | null;
}

/** Backend-parsed structured result metadata, keyed by tool. */
export interface ResultMeta {
  /** AskUserQuestion: { [question]: selectedLabel(s) }. */
  answers?: Record<string, string | string[]>;
  /** WebFetch: HTTP/timing metadata (the fetched text stays in content_preview). */
  webfetch?: WebFetchMeta;
  /** WebSearch: stats (the summary text stays in content_preview). */
  websearch?: { count?: number | null; duration_seconds?: number | null };
  /** Bash / PowerShell / Monitor / Codex shell: flags (stdout stays in content_preview). */
  terminal?: {
    interrupted?: boolean | null;
    background_task_id?: string | null;
    exit_note?: string | null;
    stderr?: string | null;
    exit_code?: number | null;
    duration_ms?: number | null;
  };
  /** Read: file info (the file body stays in content_preview). */
  read?: {
    file_path?: string | null;
    num_lines?: number | null;
    total_lines?: number | null;
    start_line?: number | null;
    is_image?: boolean;
  };
  /** Grep: match stats (the matches stay in content_preview). */
  grep?: { mode?: string | null; num_files?: number | null; num_lines?: number | null; truncated?: number | null };
  /** Glob: file-count stats (the filenames stay in content_preview). */
  glob?: { num_files?: number | null; truncated?: boolean | null };
}

export interface WebFetchMeta {
  code?: number | null;
  code_text?: string | null;
  bytes?: number | null;
  duration_ms?: number | null;
  url?: string | null;
}

/** AskUserQuestion tool_input shape. */
export interface AskQuestion {
  question: string;
  header: string;
  multiSelect?: boolean;
  options: Array<{ label: string; description?: string }>;
}

/** WebFetch tool_input shape. */
export interface WebFetchInput {
  url?: string;
  prompt?: string;
}

export interface WebSearchInput { query?: string }
export interface TerminalInput { command?: string; description?: string; run_in_background?: boolean }
export interface ReadInput { file_path?: string; offset?: number; limit?: number }
export interface GrepInput { pattern?: string; path?: string; glob?: string; output_mode?: string }
export interface GlobInput { pattern?: string; path?: string }
export interface TodoItem { content: string; status: string; activeForm?: string }
export interface TodoWriteInput { todos?: TodoItem[] }

export interface DiffInfo {
  file_path: string;
  hunks: DiffHunk[];
}

export interface DiffHunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: string[];
}

export interface PaginatedRecords {
  records: SessionRecord[];
  /** Byte offset at which this returned window starts. */
  start_byte_offset: number;
  next_byte_offset: number;
  has_more: boolean;
  /** Whether records exist before `start_byte_offset`. */
  has_earlier: boolean;
}

export interface SubagentInfo {
  agent_id: string;
  agent_type: string;
  description: string;
  tool_use_id: string;
  record_count: number;
}

export interface PriceRow {
  match: string;
  input: number;
  output: number;
}

export interface DejavuConfig {
  shell: string;
  env: Record<string, string>;
  agent_args: Record<string, string[]>;
  prices: PriceRow[];
  /** Look for agent installs inside WSL distributions. */
  wsl_scan: boolean;
  /** Distributions to skip; reading a distro's share starts it. */
  wsl_excluded: string[];
}

export interface InstructionArtifact {
  source: string;
  source_display_name: string;
  title: string;
  scope: string;
  kind: string;
  path: string;
  exists: boolean;
  editable: boolean;
  size_bytes: number;
  description: string;
}

export interface InstructionDetail extends InstructionArtifact {
  content: string;
}

export interface ProjectContextStatus {
  supported: boolean;
  writable: boolean;
  message: string;
}

export interface ProjectContext {
  source: string;
  source_display_name: string;
  project: string;
  project_path: string;
  instructions: InstructionDetail[];
  configs: InstructionDetail[];
  rules: RuleFile[];
  memories: MemoryFile[];
  memory_project: ProjectInfo | null;
  instructions_status: ProjectContextStatus;
  rules_status: ProjectContextStatus;
  memory_status: ProjectContextStatus;
}
