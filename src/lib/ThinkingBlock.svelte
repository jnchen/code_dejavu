<script lang="ts">
  import Markdown from "$lib/Markdown.svelte";
  import { t } from "$lib/i18n.svelte";

  let { content = "", highlight = "" }: { content?: string; highlight?: string } = $props();
  let expanded = $state(false);

  let preview = $derived(content.replace(/\s+/g, " ").trim().slice(0, 60));
</script>

<div class="mx-4">
  <button onclick={() => (expanded = !expanded)}
    class="flex items-center gap-1.5 w-full text-left text-[11px] text-text-muted hover:text-text-secondary
      px-2 py-1 rounded-md hover:bg-bg-hover transition-colors">
    <span class="shrink-0 opacity-70">{expanded ? "▾" : "▸"}</span>
    <span class="shrink-0">💭 {t("tool.thinking")}</span>
    {#if !expanded && preview}
      <span class="truncate opacity-50 italic">{preview}…</span>
    {/if}
  </button>
  {#if expanded}
    <div class="mt-1 ml-5 pl-3 border-l-2 border-border-subtle text-text-muted text-[11px]">
      <Markdown {content} {highlight} />
    </div>
  {/if}
</div>
