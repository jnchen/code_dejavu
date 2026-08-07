<script lang="ts">
  let { content = "", timestamp = null }: { content?: string; timestamp?: string | null } = $props();

  // First line = summary (always shown); the rest = detail (collapsed by default, never removed).
  let nl = $derived(content.indexOf("\n"));
  let summary = $derived(nl >= 0 ? content.slice(0, nl) : content);
  let detail = $derived(nl >= 0 ? content.slice(nl + 1).replace(/^\n+/, "") : "");
  let hasDetail = $derived(detail.trim().length > 0);
  let expanded = $state(false);
</script>

<div class="mx-8 rounded-lg bg-bg-tertiary/40 border border-border-subtle">
  <button
    onclick={() => hasDetail && (expanded = !expanded)}
    class="w-full flex items-center gap-1.5 px-3 py-1.5 text-left text-[11px] text-text-secondary
      {hasDetail ? 'hover:bg-bg-hover cursor-pointer' : 'cursor-default'} rounded-lg transition-colors">
    {#if hasDetail}<span class="shrink-0 opacity-70">{expanded ? "▾" : "▸"}</span>{/if}
    <span class="break-words">{summary}</span>
    {#if timestamp}<span class="ml-auto shrink-0 text-[10px] text-text-muted opacity-60">{timestamp}</span>{/if}
  </button>
  {#if expanded && hasDetail}
    <div class="px-3 pb-2 pt-0.5">
      <pre class="text-[10px] font-mono text-text-muted whitespace-pre-wrap break-words max-h-96 overflow-y-auto bg-bg-secondary rounded px-2 py-1.5 border border-border/30">{detail}</pre>
    </div>
  {/if}
</div>
