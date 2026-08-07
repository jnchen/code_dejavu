<script lang="ts">
  import type { SessionRecord, GlobInput } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let input = $derived((record.tool_input ?? {}) as GlobInput);
  let meta = $derived(result?.result_meta?.glob ?? null);
  let pattern = $derived(input.pattern ?? "");
  let numFiles = $derived(meta?.num_files ?? null);
  let truncated = $derived(meta?.truncated === true);
  // content_preview is the newline-joined file list.
  let files = $derived(
    (result?.content_preview ?? "").split("\n").map((s) => s.trim()).filter(Boolean)
  );
</script>

<div class="space-y-1.5">
  <div class="flex items-center gap-2 flex-wrap px-1">
    {#if pattern}<span class="text-[11px] font-mono text-accent break-all">{pattern}</span>{/if}
    {#if numFiles != null}
      <span class="text-[9px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted shrink-0">{t("tool.filesN", { n: numFiles })}</span>
    {/if}
    {#if truncated}<span class="text-[9px] px-1.5 py-0.5 rounded bg-warning-dim text-warning shrink-0">{t("tool.truncated")}</span>{/if}
  </div>

  {#if files.length > 0}
    <div class="rounded-md bg-bg-secondary border border-border-subtle px-3 py-2 max-h-80 overflow-auto space-y-0.5">
      {#each files as f}
        <div class="text-[10px] font-mono text-text-secondary whitespace-nowrap">{f}</div>
      {/each}
    </div>
  {:else if result}
    <div class="text-[10px] text-text-muted italic px-1">{t("tool.noMatchFiles")}</div>
  {/if}
</div>
