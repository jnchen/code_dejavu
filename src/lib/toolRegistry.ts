import type { Component } from "svelte";
import { t as tr } from "$lib/i18n.svelte";
import WebFetch from "$lib/WebFetch.svelte";
import WebSearch from "$lib/WebSearch.svelte";
import AskUserQuestion from "$lib/AskUserQuestion.svelte";
import Terminal from "$lib/Terminal.svelte";
import FileEdit from "$lib/FileEdit.svelte";
import FileRead from "$lib/FileRead.svelte";
import FileList from "$lib/FileList.svelte";
import Grep from "$lib/Grep.svelte";
import TodoWrite from "$lib/TodoWrite.svelte";
import ApplyPatch from "$lib/ApplyPatch.svelte";

export interface ToolMeta {
  label: string;
  icon: string;
  /** Dedicated body renderer. When absent, ToolCall uses its Generic body. */
  body?: Component<any>;
}

// `label` holds an i18n key, resolved to the active language in toolMeta().
const t = (label: string, icon: string, body?: Component<any>): ToolMeta => ({ label, icon, body });

// Tool name → display metadata + optional dedicated renderer.
// Adding a new tool view = one entry here + one component. Nothing else changes,
// and SubAgent's nested tools inherit it automatically (they render through ToolCall).
const REGISTRY: Record<string, ToolMeta> = {
  WebFetch: t("tool.label.webfetch", "🌐", WebFetch),
  WebSearch: t("tool.label.websearch", "🔍", WebSearch),
  AskUserQuestion: t("tool.label.askUser", "❓", AskUserQuestion),
  Bash: t("tool.label.terminal", "💻", Terminal),
  PowerShell: t("tool.label.terminal", "💻", Terminal),
  Monitor: t("tool.label.monitor", "📡", Terminal),
  BashOutput: t("tool.label.bashOutput", "💻", Terminal),
  Edit: t("tool.label.edit", "✏️", FileEdit),
  Write: t("tool.label.write", "📝", FileEdit),
  MultiEdit: t("tool.label.multiEdit", "✏️", FileEdit),
  NotebookEdit: t("tool.label.notebookEdit", "📓", FileEdit),
  Read: t("tool.label.read", "📖", FileRead),
  Glob: t("tool.label.glob", "📁", FileList),
  Grep: t("tool.label.grep", "🔎", Grep),
  TodoWrite: t("tool.label.todo", "✅", TodoWrite),
  // Codex CLI tools. shell_command is shape-compatible with the Terminal body (input.command
  // + result text). The rest use the Generic body until they earn dedicated renderers.
  shell_command: t("tool.label.terminal", "💻", Terminal),
  update_plan: t("tool.label.updatePlan", "📋"),
  apply_patch: t("tool.label.applyPatch", "🩹", ApplyPatch),
  web_search: t("tool.label.websearch", "🔍"),
  ask_user_question: t("tool.label.askUser", "❓", AskUserQuestion), // Codex request_user_input (canonicalized)
  // OpenCode tools (lowercase). bash is Terminal-compatible (input.command + output text);
  // the rest use the Generic body (input params + output) until they earn dedicated renderers.
  bash: t("tool.label.terminal", "💻", Terminal),
  interactive_bash: t("tool.label.interactiveBash", "💻", Terminal),
  background_output: t("tool.label.bgOutput", "💻", Terminal),
  read: t("tool.label.read", "📖", FileRead),
  edit: t("tool.label.edit", "✏️", ApplyPatch),
  write: t("tool.label.write", "📝", FileRead),
  glob: t("tool.label.glob", "📁", FileList),
  grep: t("tool.label.grep", "🔎", Grep),
  codesearch: t("tool.label.codesearch", "🔎"),
  webfetch: t("tool.label.webfetch", "🌐", WebFetch),
  websearch: t("tool.label.websearch", "🔍", WebSearch),
  todowrite: t("tool.label.todo", "✅", TodoWrite),
  task: t("tool.label.subagent", "🤖"),
  question: t("tool.label.askUser", "❓", AskUserQuestion),
  patch: t("tool.label.patch", "🩹"),
  skill: t("tool.label.skill", "⚡"),
  // Known tools that look fine in the Generic body but deserve a friendly label/icon.
  Agent: t("tool.label.subagent", "🤖"),
  Task: t("tool.label.subagent", "🤖"),
  ExitPlanMode: t("tool.label.exitPlan", "📋"),
  Workflow: t("tool.label.workflow", "🔀"),
  ToolSearch: t("tool.label.toolSearch", "🧰"),
  ScheduleWakeup: t("tool.label.scheduleWakeup", "⏰"),
  TaskCreate: t("tool.label.taskCreate", "🗂️"),
  TaskUpdate: t("tool.label.taskUpdate", "🗂️"),
  TaskGet: t("tool.label.taskGet", "🗂️"),
  TaskStop: t("tool.label.taskStop", "🛑"),
  Skill: t("tool.label.skill", "⚡"),
  SlashCommand: t("tool.label.slashCommand", "⚡"),
};

const FALLBACK: ToolMeta = { label: "tool.label.fallback", icon: "🔧" };

// Registry labels are i18n keys; resolve them to the active language here (call-time, so a
// consumer's $derived re-runs when the language toggles). mcp / unknown tools keep their literal name.
export function toolMeta(name: string | null | undefined): ToolMeta {
  if (!name) return { ...FALLBACK, label: tr(FALLBACK.label) };
  const hit = REGISTRY[name];
  if (hit) return { ...hit, label: tr(hit.label) };
  if (name.startsWith("mcp__")) {
    return { label: mcpLabel(name), icon: "🔌" };
  }
  return { label: name, icon: "🔧" };
}

// mcp__server__tool  ->  "server · tool"
function mcpLabel(name: string): string {
  return name.replace(/^mcp__/, "").split("__").join(" · ");
}
