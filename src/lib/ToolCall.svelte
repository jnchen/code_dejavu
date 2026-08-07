<script lang="ts">
  import type { SessionRecord } from "$lib/types";
  import { t } from "$lib/i18n.svelte";
  import { toolMeta } from "$lib/toolRegistry";
  import GenericTool from "$lib/GenericTool.svelte";
  import MetaBlock from "$lib/MetaBlock.svelte";

  let {
    record,
    result = null,
    expanded = false,
    onToggle,
    highlight = "",
    toolName = null,
    callRecord = null,
    preHooks = null,
    postHooks = null,
  }: {
    record: SessionRecord;
    result?: SessionRecord | null;
    expanded?: boolean;
    onToggle?: () => void;
    highlight?: string;
    /** Tool name for a result card (whose own record carries no tool_name). */
    toolName?: string | null;
    /** The matching tool_use call record (so a result card can show what was called + its id). */
    callRecord?: SessionRecord | null;
    /** PreToolUse hooks (rendered above the result) / PostToolUse hooks (below). */
    preHooks?: SessionRecord[] | null;
    postHooks?: SessionRecord[] | null;
  } = $props();

  let isResult = $derived(record.record_type === "tool_result");
  // Result cards borrow the call's name/input so they show what they're the result OF.
  let callInput = $derived(
    (record.tool_input ?? callRecord?.tool_input ?? null) as Record<string, unknown> | null
  );
  let name = $derived(record.tool_name ?? callRecord?.tool_name ?? toolName ?? null);
  let meta = $derived(toolMeta(name));
  let Body = $derived(meta.body ?? null);

  // Short id linking a call to its result (matches across parallel calls).
  let idShort = $derived((record.tool_use_id ?? callRecord?.tool_use_id ?? "").slice(-5));

  let filePath = $derived(
    callInput?.file_path
      ? String(callInput.file_path).split(/[/\\]/).slice(-2).join("/")
      : null
  );

  // One-line summary in the collapsed header (full detail lives in the expanded body).
  // For a result card this is the CALL's input, so you can tell which call it belongs to.
  let summary = $derived.by(() => {
    if (filePath) return filePath;
    if (callInput) {
      const v = callInput.url ?? callInput.command ?? callInput.pattern ?? callInput.query ?? callInput.path ?? callInput.prompt;
      if (v != null) return String(v).split("\n")[0];
    }
    if (isResult) {
      const txt = (record.content_preview ?? "").trim();
      return txt ? txt.split("\n")[0] : null;
    }
    return null;
  });
</script>

<div class="mx-1">
  <!-- Header bar = collapse toggle -->
  <button onclick={onToggle}
    class="w-full flex items-center gap-2 px-3 py-1.5 rounded-lg bg-bg-tertiary border border-border-subtle
      text-[11px] text-left hover:bg-bg-hover transition-colors {expanded ? 'rounded-b-none' : ''}">
    <span class="shrink-0 opacity-50 text-[9px] w-2">{expanded ? "▾" : "▸"}</span>
    <span class="w-1.5 h-1.5 rounded-full {isResult ? 'bg-success' : 'bg-accent'} shrink-0"></span>
    <span class="font-medium text-text-secondary shrink-0">{meta.icon} {meta.label}</span>
    {#if isResult}
      <span class="text-[9px] px-1.5 py-0.5 rounded bg-success-dim text-success shrink-0">{t("tool.result")}</span>
    {/if}
    {#if idShort}
      <span class="text-[9px] font-mono text-text-muted/70 shrink-0" title={record.tool_use_id ?? callRecord?.tool_use_id ?? ''}>#{idShort}</span>
    {/if}
    {#if summary}
      <!-- Title preview: ONE line, ellipsis when too long (省略 ok here). The full, never-
           truncated content lives in the expanded body. -->
      <span class="font-mono text-text-muted truncate min-w-0 flex-1">{summary}</span>
    {/if}
    {#if record.timestamp}
      <span class="text-[10px] text-text-muted ml-auto shrink-0">{record.timestamp}</span>
    {/if}
  </button>

  <!-- Body (only when expanded): Pre hooks → input/result → Post hooks, all in one card -->
  {#if expanded}
    <div class="border-x border-b border-border-subtle rounded-b-lg overflow-hidden">
      {#if preHooks && preHooks.length}
        <div class="px-2 py-1.5 bg-bg-tertiary/10 space-y-1 border-b border-border-subtle">
          {#each preHooks as h}<MetaBlock content={h.content_preview} timestamp={h.timestamp} />{/each}
        </div>
      {/if}
      {#if Body}
        <div class="px-2.5 py-2 bg-bg-tertiary/20"><Body {record} {result} {highlight} /></div>
      {:else}
        <GenericTool {record} {result} expanded={true} {highlight} />
      {/if}
      {#if postHooks && postHooks.length}
        <div class="px-2 py-1.5 bg-bg-tertiary/10 space-y-1 border-t border-border-subtle">
          {#each postHooks as h}<MetaBlock content={h.content_preview} timestamp={h.timestamp} />{/each}
        </div>
      {/if}
    </div>
  {/if}
</div>
