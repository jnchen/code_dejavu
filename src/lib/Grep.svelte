<script lang="ts">
  import type { SessionRecord, GrepInput } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let input = $derived((record.tool_input ?? {}) as GrepInput);
  let meta = $derived(result?.result_meta?.grep ?? null);
  let pattern = $derived(input.pattern ?? "");
  let path = $derived(input.path ?? "");
  let glob = $derived(input.glob ?? "");
  let mode = $derived(meta?.mode ?? "");
  let numFiles = $derived(meta?.num_files ?? null);
  let numLines = $derived(meta?.num_lines ?? null);
  let body = $derived(result?.content_preview ?? "");
</script>

<div class="space-y-1.5">
  <div class="flex items-center gap-2 flex-wrap px-1">
    {#if pattern}<span class="text-[11px] font-mono text-accent break-all">/{pattern}/</span>{/if}
    {#if glob}<span class="text-[9px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted shrink-0">{glob}</span>{/if}
    {#if path}<span class="text-[10px] font-mono text-text-muted truncate">{path}</span>{/if}
  </div>

  {#if numFiles != null || numLines != null || mode}
    <div class="flex items-center gap-2 flex-wrap px-1 text-[9px] text-text-muted">
      {#if mode}<span class="px-1.5 py-0.5 rounded bg-bg-tertiary">{mode}</span>{/if}
      {#if numFiles != null}<span>{t("tool.filesN", { n: numFiles })}</span>{/if}
      {#if numLines != null}<span>{t("tool.linesMatched", { n: numLines })}</span>{/if}
    </div>
  {/if}

  {#if body}
    <div class="rounded-md bg-bg-secondary border border-border-subtle px-3 py-2 max-h-96 overflow-auto">
      <pre class="text-[10px] font-mono text-text-secondary whitespace-pre leading-snug">{body}</pre>
    </div>
  {:else if result}
    <div class="text-[10px] text-text-muted italic px-1">{t("tool.noMatch")}</div>
  {/if}
</div>
