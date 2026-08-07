<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import Markdown from "$lib/Markdown.svelte";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import type { WorkflowItem } from "$lib/types";

  let items = $state<WorkflowItem[]>([]);
  let loading = $state(true);
  let error = $state("");
  let kind = $state<string | null>(null);
  let search = $state("");
  let selected = $state<WorkflowItem | null>(null);
  let content = $state("");
  let loadingContent = $state(false);

  const KINDS = ["skill", "command", "plan", "task"];

  let kindCounts = $derived.by(() => {
    const counts: Record<string, number> = {};
    for (const it of items) counts[it.kind] = (counts[it.kind] ?? 0) + 1;
    return counts;
  });

  let filtered = $derived(
    items.filter((it) => {
      if (kind && it.kind !== kind) return false;
      if (search.trim()) {
        const q = search.toLowerCase();
        return it.name.toLowerCase().includes(q) || it.description.toLowerCase().includes(q);
      }
      return true;
    })
  );

  async function load() {
    loading = true;
    error = "";
    selected = null;
    try {
      items = await api.workflows.list();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function open(it: WorkflowItem) {
    selected = it;
    content = "";
    loadingContent = true;
    try {
      content = await api.workflows.read(it.source, it.path);
    } catch (e) {
      error = String(e);
    } finally {
      loadingContent = false;
    }
  }

  function kindLabel(k: string): string {
    return KINDS.includes(k) ? t("wf." + k) : k;
  }

  function formatSize(bytes: number): string {
    if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
  }

  onMount(() => deferRouteLoad(load));
</script>

<div class="p-6 h-full flex flex-col">
  <div class="mb-4 flex items-center gap-3 shrink-0">
    <h2 class="text-lg font-semibold shrink-0">{t("wf.title")}</h2>
    {#if !selected}
      <div class="relative flex-1">
        <input
          bind:value={search}
          placeholder={t("wf.searchPlaceholder")}
          class="w-full px-3 py-1.5 text-xs bg-bg-secondary border border-border rounded-lg focus:border-accent outline-none transition-all"
        />
      </div>
      <button
        onclick={load}
        disabled={loading}
        title={t("wf.rescan")}
        class="shrink-0 rounded-lg border border-border px-2.5 py-1.5 text-[11px] text-text-secondary hover:bg-bg-hover transition-colors disabled:opacity-50"
      >
        {loading ? t("wf.scanning") : t("common.refresh")}
      </button>
    {/if}
  </div>

  {#if error}
    <div class="mb-4 p-3 bg-danger-dim border border-danger/30 rounded-xl text-sm text-danger shrink-0">{error}</div>
  {/if}

  {#if selected}
    <div class="mb-3 flex items-center justify-between gap-2 shrink-0">
      <button onclick={() => (selected = null)} class="text-xs text-accent hover:underline shrink-0">{t("wf.back")}</button>
      <button
        onclick={() => selected && api.shell.revealPath(selected.path)}
        class="rounded-lg border border-border px-3 py-1.5 text-[11px] hover:bg-bg-hover transition-colors"
      >{t("wf.openInFiles")}</button>
    </div>
    <div class="bg-bg-secondary border border-border rounded-xl p-4 mb-3 shrink-0">
      <div class="flex items-center gap-2">
        <span class="rounded-full bg-accent-dim px-2 py-0.5 text-[10px] text-accent">{kindLabel(selected.kind)}</span>
        <span class="text-sm font-medium truncate">{selected.name}</span>
      </div>
      <div class="mt-1 truncate font-mono text-[10px] text-text-muted">{selected.path}</div>
    </div>
    <div class="flex-1 overflow-y-auto rounded-xl border border-border bg-bg-secondary p-5">
      {#if loadingContent}
        <p class="text-xs text-text-secondary">{t("common.loading")}</p>
      {:else}
        <Markdown content={content || t("common.emptyFile")} />
      {/if}
    </div>
  {:else}
    <div class="mb-3 flex flex-wrap items-center gap-1.5 shrink-0">
      <button
        onclick={() => (kind = null)}
        class="text-[11px] px-2.5 py-1 rounded-lg transition-colors {kind === null ? 'bg-accent text-white' : 'bg-bg-secondary hover:bg-bg-hover text-text-secondary'}"
      >{t("wf.all")} {items.length}</button>
      {#each KINDS as k}
        {#if kindCounts[k]}
          <button
            onclick={() => (kind = kind === k ? null : k)}
            class="text-[11px] px-2.5 py-1 rounded-lg transition-colors {kind === k ? 'bg-accent text-white' : 'bg-bg-secondary hover:bg-bg-hover text-text-secondary'}"
          >{kindLabel(k)} {kindCounts[k]}</button>
        {/if}
      {/each}
    </div>

    {#if loading}
      <p class="text-text-secondary text-sm">{t("wf.scanningLocal")}</p>
    {:else if filtered.length === 0}
      <div class="flex flex-col items-center justify-center flex-1 text-text-muted">
        <p class="text-sm">{items.length === 0 ? t("wf.none") : t("wf.noMatch")}</p>
        {#if items.length === 0}
          <p class="text-xs mt-1">{t("wf.noneHint")}</p>
        {/if}
      </div>
    {:else}
      <div class="flex-1 overflow-y-auto space-y-1.5">
        {#each filtered as it}
          <button
            onclick={() => open(it)}
            class="w-full text-left p-3 bg-bg-secondary border border-border rounded-xl hover:border-border-hover transition-colors"
          >
            <div class="flex items-center gap-2">
              <span class="shrink-0 rounded-full bg-bg-tertiary px-1.5 py-0.5 text-[9px] text-text-muted">{kindLabel(it.kind)}</span>
              <span class="text-xs font-medium truncate">{it.name}</span>
              <span class="ml-auto shrink-0 text-[10px] text-text-muted">{formatSize(it.size_bytes)}</span>
            </div>
            {#if it.description}
              <p class="mt-1 text-[11px] text-text-secondary line-clamp-2">{it.description}</p>
            {/if}
          </button>
        {/each}
      </div>
    {/if}
  {/if}
</div>
