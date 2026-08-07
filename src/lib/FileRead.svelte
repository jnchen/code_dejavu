<script lang="ts">
  import type { SessionRecord, ReadInput } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let input = $derived((record.tool_input ?? {}) as ReadInput);
  let meta = $derived(result?.result_meta?.read ?? null);
  let filePath = $derived(meta?.file_path ?? input.file_path ?? "");
  let shortPath = $derived(filePath ? filePath.split(/[/\\]/).slice(-3).join("/") : "");
  let numLines = $derived(meta?.num_lines ?? null);
  let totalLines = $derived(meta?.total_lines ?? null);
  let startLine = $derived(meta?.start_line ?? null);
  let isImage = $derived(meta?.is_image === true);
  let body = $derived(result?.content_preview ?? "");
</script>

<div class="space-y-1.5">
  <div class="flex items-center gap-2 flex-wrap px-1">
    {#if shortPath}<span class="text-[10px] font-mono text-text-secondary break-all">{shortPath}</span>{/if}
    {#if numLines != null}
      <span class="text-[9px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted shrink-0">
        {t("tool.linesN", { n: numLines })}{#if totalLines != null && totalLines !== numLines}{t("tool.ofTotal", { n: totalLines })}{/if}{#if startLine != null && startLine > 1}{t("tool.fromLine", { n: startLine })}{/if}
      </span>
    {/if}
  </div>

  {#if isImage}
    <div class="text-[11px] text-text-muted italic px-1">🖼 {t("tool.imageFile")}</div>
  {:else if body}
    <div class="rounded-md bg-bg-secondary border border-border-subtle px-3 py-2 max-h-96 overflow-auto">
      <pre class="text-[11px] font-mono text-text-secondary whitespace-pre leading-snug">{body}</pre>
    </div>
  {/if}
</div>
