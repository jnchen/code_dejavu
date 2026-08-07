<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import type { ToolsInfo } from "$lib/types";

  let info = $state<ToolsInfo | null>(null);
  let loading = $state(true);
  let error = $state("");
  let source = $state("");
  let active = $derived(info?.sources.find((item) => item.source === source) ?? info?.sources[0] ?? null);

  async function load() {
    loading = true;
    error = "";
    try {
      info = await api.tools.list();
      if (!info.sources.some((item) => item.source === source)) {
        source = info.sources.find((item) => item.available)?.source ?? info.sources[0]?.source ?? "";
      }
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function transportClass(t: string): string {
    if (t === "stdio") return "bg-accent-dim text-accent";
    if (t === "http" || t === "sse") return "bg-success-dim text-success";
    return "bg-bg-tertiary text-text-muted";
  }

  onMount(() => deferRouteLoad(load));
</script>

<div class="p-6 h-full flex flex-col">
  <div class="mb-4 flex items-center gap-3 shrink-0">
    <h2 class="text-lg font-semibold shrink-0">{t("tools.title")}</h2>
    <p class="text-xs text-text-muted flex-1">{t("tools.subtitle")}</p>
    <button
      onclick={load}
      disabled={loading}
      class="shrink-0 rounded-lg border border-border px-2.5 py-1.5 text-[11px] text-text-secondary hover:bg-bg-hover transition-colors disabled:opacity-50"
    >{loading ? t("tools.reading") : t("common.refresh")}</button>
  </div>

  {#if info && info.sources.length > 0}
    <div class="mb-4 flex shrink-0 overflow-hidden rounded-lg border border-border self-start">
      {#each info.sources as item}
        <button
          onclick={() => (source = item.source)}
          disabled={!item.available}
          class="border-l border-border px-3 py-1.5 text-[11px] first:border-l-0 transition-colors
            {source === item.source ? 'bg-accent text-white' : item.available ? 'hover:bg-bg-hover' : 'cursor-not-allowed text-text-muted opacity-50'}"
        >
          {item.source_display_name}
          <span class="ml-1 text-[9px] opacity-75">{item.mcp_servers.length + item.hooks.length}</span>
        </button>
      {/each}
    </div>
  {/if}

  {#if error}
    <div class="mb-4 p-3 bg-danger-dim border border-danger/30 rounded-xl text-sm text-danger shrink-0">{error}</div>
  {/if}

  {#if loading}
    <p class="text-text-secondary text-sm">{t("tools.loadingLocal")}</p>
  {:else if active}
    <div class="flex-1 overflow-y-auto space-y-6">
      <!-- MCP servers -->
      <section>
        <div class="mb-2 flex items-center justify-between gap-2">
          <h3 class="text-sm font-semibold">{t("tools.mcpServers")} <span class="text-text-muted font-normal">{active.mcp_servers.length}</span></h3>
          {#if active.mcp_source_paths.length > 0}
            <div class="flex max-w-xl flex-wrap justify-end gap-x-2">
              {#each active.mcp_source_paths as path}
                <button onclick={() => api.shell.revealPath(path)}
                  class="max-w-md truncate text-[10px] text-text-muted hover:text-accent" title={path}>
                  {path}
                </button>
              {/each}
            </div>
          {/if}
        </div>
        {#if active.mcp_servers.length === 0}
          <div class="rounded-xl border border-border bg-bg-secondary p-4 text-xs text-text-secondary">{t("tools.noMcp")}</div>
        {:else}
          <div class="space-y-2">
            {#each active.mcp_servers as s}
              <div class="rounded-xl border border-border bg-bg-secondary p-3">
                <div class="flex items-center gap-2 flex-wrap">
                  <span class="text-xs font-medium">{s.name}</span>
                  <span class="rounded-full px-1.5 py-0.5 text-[9px] {transportClass(s.transport)}">{s.transport}</span>
                  {#if !s.enabled}<span class="rounded-full bg-danger-dim px-1.5 py-0.5 text-[9px] text-danger">{t("tools.disabled")}</span>{/if}
                  <span class="rounded-full bg-bg-tertiary px-1.5 py-0.5 text-[9px] text-text-muted">{s.scope === "global" ? t("tools.global") : s.scope}</span>
                </div>
                {#if s.command}
                  <div class="mt-1.5 break-all font-mono text-[11px] text-text-secondary">
                    {s.command}{#if s.args.length > 0} {s.args.join(" ")}{/if}
                  </div>
                {/if}
                {#if s.env_keys.length > 0}
                  <div class="mt-1.5 flex flex-wrap items-center gap-1">
                    <span class="text-[10px] text-text-muted">env:</span>
                    {#each s.env_keys as k}
                      <span class="rounded bg-bg-tertiary px-1.5 py-0.5 font-mono text-[9px] text-text-muted">{k}</span>
                    {/each}
                    <span class="text-[9px] text-text-muted">{t("tools.envHidden")}</span>
                  </div>
                {/if}
              </div>
            {/each}
          </div>
        {/if}
      </section>

      <!-- Hooks -->
      <section>
        <div class="mb-2 flex items-center justify-between gap-2">
          <h3 class="text-sm font-semibold">{t("tools.hooks")} <span class="text-text-muted font-normal">{active.hooks.length}</span></h3>
          {#if active.hooks_source_paths.length > 0}
            <div class="flex max-w-xl flex-wrap justify-end gap-x-2">
              {#each active.hooks_source_paths as path}
                <button onclick={() => api.shell.revealPath(path)}
                  class="max-w-md truncate text-[10px] text-text-muted hover:text-accent" title={path}>
                  {path}
                </button>
              {/each}
            </div>
          {/if}
        </div>
        {#if active.hooks.length === 0}
          <div class="rounded-xl border border-border bg-bg-secondary p-4 text-xs text-text-secondary">{t("tools.noHooks")}</div>
        {:else}
          <div class="space-y-2">
            {#each active.hooks as h}
              <div class="rounded-xl border border-border bg-bg-secondary p-3">
                <div class="flex items-center gap-2">
                  <span class="rounded-full bg-accent-dim px-2 py-0.5 text-[10px] text-accent">{h.event}</span>
                  {#if h.matcher}
                    <span class="font-mono text-[10px] text-text-muted">matcher: {h.matcher}</span>
                  {/if}
                </div>
                {#each h.commands as c}
                  <div class="mt-1.5 break-all rounded-lg bg-bg p-2 font-mono text-[11px] text-text-secondary">{c}</div>
                {/each}
              </div>
            {/each}
          </div>
        {/if}
      </section>
    </div>
  {/if}
</div>
