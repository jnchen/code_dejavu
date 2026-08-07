<script lang="ts">
  import type { SessionRecord, WebSearchInput } from "$lib/types";
  import Markdown from "$lib/Markdown.svelte";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
    result = null,
    highlight = "",
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let input = $derived((record.tool_input ?? {}) as WebSearchInput);
  let meta = $derived(result?.result_meta?.websearch ?? null);
  let query = $derived(input.query ?? "");
  let count = $derived(meta?.count ?? null);
  let duration = $derived(meta?.duration_seconds ?? null);
  let resultText = $derived(result?.content_preview ?? "");
</script>

<div class="space-y-2">
  <!-- The search query -->
  {#if query}
    <div class="flex items-center gap-2">
      <span class="text-[11px] shrink-0">🔍</span>
      <span class="text-[12px] font-medium text-text break-words">{query}</span>
    </div>
  {/if}

  <!-- The result summary (links are usually inline markdown) -->
  {#if resultText}
    <div class="rounded-md bg-bg-secondary border border-border-subtle px-3 py-2 max-h-96 overflow-y-auto">
      <Markdown content={resultText} {highlight} />
    </div>
  {/if}

  <!-- Stats footer -->
  {#if count != null || duration != null}
    <div class="flex items-center gap-3 text-[10px] text-text-muted px-1">
      {#if count != null}<span>🔗 {t("tool.resultsN", { n: count })}</span>{/if}
      {#if duration != null}<span>⏱ {duration.toFixed(1)} s</span>{/if}
    </div>
  {/if}
</div>
