<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import Markdown from "$lib/Markdown.svelte";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import type { RuleFile, SourceInfo } from "$lib/types";

  let sources = $state<SourceInfo[]>([]);
  let allRules = $state<RuleFile[]>([]);
  let selectedSource = $state<string | null>(null);
  let loading = $state(true);
  let error = $state("");
  let selected = $state<RuleFile | null>(null);
  let toggling = $state<string | null>(null);

  let rules = $derived(
    selectedSource ? allRules.filter((rule) => rule.source === selectedSource) : allRules
  );

  let categories = $derived(
    [...new Set(rules.map((r) => r.category))].sort()
  );

  async function refresh() {
    loading = true;
    try {
      const [sourceList, ruleList] = await Promise.all([
        api.sessions.listSources(),
        api.rules.list(null),
      ]);
      sources = sourceList;
      allRules = ruleList.filter((rule) => rule.scope !== "project");
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function toggle(r: RuleFile) {
    if (!canToggleRule(r)) return;
    toggling = r.filename;
    try {
      await api.rules.toggle(r.category, r.filename, !r.enabled, r.source);
      await refresh();
    } catch (e) {
      error = String(e);
    } finally {
      toggling = null;
    }
  }

  function rulesInCategory(cat: string) {
    return rules.filter((r) => r.category === cat);
  }

  function selectedSourceName(): string {
    if (!selectedSource) return t("rules.all");
    return sources.find((source) => source.id === selectedSource)?.display_name ?? t("rules.fallback");
  }

  function sourceCanWriteRules(sourceId?: string | null): boolean {
    return sources.find((source) => source.id === sourceId)?.capabilities.rules_write ?? false;
  }

  function canToggleRule(rule: RuleFile): boolean {
    return selectedSource != null && rule.toggleable && sourceCanWriteRules(rule.source);
  }

  function sourceRuleCount(sourceId: string): number {
    return allRules.filter((rule) => rule.source === sourceId).length;
  }

  function changeSource(source: string | null) {
    selectedSource = source;
    selected = null;
  }

  let ruleSources = $derived(
    sources.filter((source) => source.capabilities.rules_read && sourceRuleCount(source.id) > 0)
  );

  onMount(() => deferRouteLoad(refresh));
</script>

<div class="flex h-full">
  <div class="w-72 shrink-0 border-r border-border overflow-y-auto">
    <div class="p-3 border-b border-border">
      <div class="flex items-center justify-between gap-2">
        <h3 class="text-xs font-medium text-text-secondary uppercase tracking-wider">{t("rules.heading", { name: selectedSourceName(), n: rules.length })}</h3>
        {#if selectedSource && !rules.some((rule) => canToggleRule(rule))}
          <span class="rounded-md bg-bg-tertiary px-1.5 py-0.5 text-[10px] text-text-muted">{t("common.readonly")}</span>
        {/if}
      </div>
      {#if ruleSources.length > 0}
        <div class="mt-2 flex rounded-lg border border-border overflow-hidden">
          <button
            onclick={() => changeSource(null)}
            class="flex-1 px-2 py-1 text-[10px] transition-colors
              {selectedSource === null ? 'bg-accent text-white' : 'hover:bg-bg-hover text-text-secondary'}"
          >
            {t("rules.allN", { n: allRules.length })}
          </button>
          {#each ruleSources as source}
            <button
              onclick={() => changeSource(source.id)}
              disabled={!source.available}
              class="flex-1 px-2 py-1 text-[10px] transition-colors
                {selectedSource === source.id ? 'bg-accent text-white' : source.available ? 'hover:bg-bg-hover text-text-secondary' : 'text-text-muted opacity-50'}"
            >
              {source.display_name} {sourceRuleCount(source.id)}
            </button>
          {/each}
        </div>
      {/if}
    </div>
    {#if loading}
      <p class="p-3 text-xs text-text-secondary">{t("common.loading")}</p>
    {:else}
      {#if categories.length === 0}
        <p class="p-3 text-xs text-text-secondary">{t("rules.noGlobal")}</p>
      {/if}
      {#each categories as cat}
        <div class="border-b border-border-subtle">
          <div class="px-3 py-1.5 text-[10px] text-text-muted uppercase bg-bg-tertiary">{cat}</div>
          {#each rulesInCategory(cat) as r}
            <div class="flex items-center gap-2 px-3 py-1.5 hover:bg-bg-hover group
              {selected?.filename === r.filename && selected?.category === r.category ? 'bg-bg-tertiary' : ''}">
              <button
                onclick={() => toggle(r)}
                disabled={toggling === r.filename || !canToggleRule(r)}
                title={selectedSource == null ? t("rules.allReadonly") : canToggleRule(r) ? t("rules.toggle") : t("rules.ruleReadonly")}
                class="w-8 h-4 rounded-full relative transition-colors
                  {r.enabled ? 'bg-success' : 'bg-border'} {canToggleRule(r) ? '' : 'opacity-60 cursor-not-allowed'}"
                aria-label={t("rules.toggleAria", { name: r.filename })}
              >
                <span class="absolute top-0.5 w-3 h-3 rounded-full bg-white shadow transition-transform
                  {r.enabled ? 'left-4' : 'left-0.5'}"></span>
              </button>
              <button
                onclick={() => (selected = r)}
                class="flex-1 text-left text-xs truncate {r.enabled ? 'text-text' : 'text-text-muted'}">
                {r.filename}
              </button>
              <span class="rounded bg-bg px-1.5 py-0.5 text-[9px] text-text-muted">{t("common.global")}</span>
            </div>
          {/each}
        </div>
      {/each}
    {/if}
  </div>

  <div class="flex-1 overflow-y-auto">
    {#if error}
      <div class="m-4 p-3 bg-danger-dim border border-danger/30 rounded-xl text-sm text-danger">{error}</div>
    {/if}
    {#if selected}
      <div class="p-4">
        <div class="flex items-center gap-2 mb-3">
          <h3 class="text-sm font-medium">{selected.category}/{selected.filename}</h3>
          <span class="text-[10px] px-1.5 py-0.5 rounded {selected.enabled ? 'bg-success-dim text-success' : 'bg-bg-tertiary text-text-muted'}">
            {selected.enabled ? t("rules.enabled") : t("rules.disabled")}
          </span>
          {#if !selected.toggleable}
            <span class="text-[10px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted">{t("common.readonly")}</span>
          {/if}
          <span class="text-[10px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted">{t("common.global")}</span>
        </div>
        <div class="bg-bg-secondary border border-border rounded-lg p-4 overflow-y-auto">
          <Markdown content={selected.content} />
        </div>
      </div>
    {:else}
      <div class="flex items-center justify-center h-full text-sm text-text-secondary">
        {t("rules.selectRule")}
      </div>
    {/if}
  </div>
</div>
