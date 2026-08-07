<script lang="ts">
  import { onMount } from "svelte";
  import { dashboardStore } from "$lib/dashboardStore.svelte";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";

  const ACTIVITY_DAYS = 30;

  let summary = $derived(dashboardStore.summary);
  let loading = $derived(dashboardStore.loading);
  let error = $derived(dashboardStore.error);
  let indexStatus = $derived(dashboardStore.indexStatus);
  let indexBuilding = $derived(dashboardStore.indexBuilding);

  let rebuilding = $state(false);

  function refresh() {
    dashboardStore.refresh();
  }

  async function rebuildIndex() {
    if (rebuilding) return;
    rebuilding = true;
    try {
      await dashboardStore.rebuildIndex();
    } finally {
      rebuilding = false;
    }
  }

  let recentSessions = $derived(summary?.recent ?? []);

  // Merge backend per-source counts with availability/names from list_sources, so the console
  // shows backend health at a glance (including sources that have no sessions yet).
  let sourceHealth = $derived(
    dashboardStore.sources.map((src) => {
      const stat = summary?.by_source.find((s) => s.source === src.id);
      return {
        id: src.id,
        name: src.display_name,
        available: src.available,
        count: stat?.count ?? 0,
        lastActive: stat?.last_active ?? "",
      };
    })
  );

  let activity = $derived(summary?.activity ?? []);
  let activityMax = $derived(Math.max(1, ...activity.map((d) => d.count)));
  let activityTotal = $derived(activity.reduce((sum, d) => sum + d.count, 0));

  let topProjects = $derived(
    (summary?.top_projects ?? []).map((p) => ({ path: p.path, count: p.count, lastActive: p.last_active }))
  );

  function sourceLabel(sourceId?: string | null): string {
    if (!sourceId) return t("dash.unknownSource");
    return dashboardStore.sources.find((source) => source.id === sourceId)?.display_name ?? sourceId;
  }

  function formatSize(bytes: number): string {
    if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
  }

  onMount(() => {
    return deferRouteLoad(() => {
      dashboardStore.hydrate();
      dashboardStore.refresh();
    });
  });
</script>

<div class="min-h-full bg-bg">
  <header class="border-b border-border bg-bg-secondary px-7 py-5">
    <div class="flex items-start justify-between gap-6">
      <div>
        <h1 class="text-2xl font-semibold tracking-tight text-text">{t("dash.title")}</h1>
        <p class="mt-1 max-w-2xl text-sm text-text-secondary">
          {t("dash.subtitle")}
        </p>
      </div>
      <button
        onclick={refresh}
        disabled={loading}
        class="shrink-0 rounded-lg border border-border bg-bg px-3 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:border-border-hover hover:bg-bg-hover disabled:opacity-40"
      >
        {loading ? t("dash.refreshing") : t("dash.refresh")}
      </button>
    </div>
  </header>

  <main class="space-y-6 px-7 py-6">
    {#if error}
      <div class="rounded-lg border border-danger/25 bg-danger-dim px-4 py-3 text-sm text-danger">{error}</div>
    {/if}

    <section class="flex items-center justify-between rounded-lg border px-4 py-3 {indexBuilding ? 'border-warning/35 bg-warning-dim' : 'border-border bg-bg-secondary'}">
      <div class="flex min-w-0 items-start gap-3">
        {#if indexBuilding}
          <span class="mt-0.5 h-3.5 w-3.5 shrink-0 animate-spin rounded-full border-2 border-warning/30 border-t-warning"></span>
        {/if}
        <div class="min-w-0">
        <h2 class="text-sm font-semibold">{t("dash.searchIndex")}</h2>
        <p class="mt-1 text-xs text-text-muted">
          {#if !indexStatus}
            {t("dash.statusUnknown")}
          {:else if indexStatus.Building !== undefined && indexStatus.Ready === undefined}
            {t("sess.indexBuildNoticeBody")}
          {:else if indexStatus.Ready}
            {t("dash.indexed", { n: indexStatus.Ready.session_count, t: indexStatus.Ready.token_count })}{#if indexStatus.Ready.failed_files > 0} · <span class="text-warning">{t("dash.failed", { k: indexStatus.Ready.failed_files })}</span>{/if}
          {:else}
            {t("sess.indexBuildNoticeBody")}
          {/if}
        </p>
        </div>
      </div>
      <button
        onclick={rebuildIndex}
        disabled={rebuilding}
        title={t("dash.rebuildTitle")}
        class="shrink-0 rounded-lg border border-border bg-bg px-3 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:border-border-hover hover:bg-bg-hover disabled:opacity-40"
      >
        {rebuilding ? t("dash.rebuilding") : t("dash.rebuild")}
      </button>
    </section>

    <section class="rounded-lg border border-border bg-bg-secondary px-4 py-3">
      <h2 class="text-sm font-semibold">{t("dash.sourceStatus")}</h2>
      <div class="mt-3 flex flex-wrap gap-2">
        {#each sourceHealth as h}
          <div class="flex items-center gap-2 rounded-lg border border-border bg-bg px-3 py-2">
            <span class="h-2 w-2 shrink-0 rounded-full {h.available ? 'bg-success' : 'bg-text-muted'}"></span>
            <div class="leading-tight">
              <div class="text-xs font-medium">{h.name}</div>
              <div class="text-[10px] text-text-muted">
                {#if h.available}{t("dash.sessionsN", { n: h.count })}{#if h.lastActive} · {h.lastActive}{/if}{:else}{t("dash.notFound")}{/if}
              </div>
            </div>
          </div>
        {/each}
        {#if sourceHealth.length === 0}
          <span class="text-xs text-text-secondary">{loading ? t("dash.loading") : t("dash.noSources")}</span>
        {/if}
      </div>
    </section>

    <section class="rounded-lg border border-border bg-bg-secondary px-4 py-3">
      <div class="flex items-center justify-between">
        <h2 class="text-sm font-semibold">{t("dash.activity")}</h2>
        <span class="text-[10px] text-text-muted">{t("dash.activityMeta", { d: ACTIVITY_DAYS, n: activityTotal })}</span>
      </div>
      <div class="mt-3 flex h-16 items-end gap-[2px]">
        {#each activity as d}
          <div class="flex-1" title={`${d.day}: ${d.count}`}>
            <div
              class="w-full rounded-t-sm bg-accent/60 transition-colors hover:bg-accent"
              style="height: {Math.max(2, Math.round((d.count / activityMax) * 56))}px"
            ></div>
          </div>
        {/each}
      </div>
    </section>

    <section class="rounded-lg border border-border bg-bg-secondary">
      <div class="flex items-center justify-between border-b border-border px-4 py-3">
        <div>
          <h2 class="text-sm font-semibold">{t("dash.recent")}</h2>
          <p class="mt-1 text-xs text-text-muted">{t("dash.recentSub")}</p>
        </div>
        <a href="/sessions" class="rounded-lg border border-border bg-bg px-3 py-1.5 text-xs text-text-secondary hover:bg-bg-hover">
          {t("dash.viewAll")}
        </a>
      </div>
      <div class="divide-y divide-border-subtle">
        {#each recentSessions as session}
          <a href="/sessions" class="block px-4 py-3 transition-colors hover:bg-bg-hover">
            <div class="flex items-start justify-between gap-4">
              <div class="min-w-0">
                <div class="truncate text-xs font-medium">{session.agent_title || session.first_prompt || t("dash.unnamed")}</div>
                <div class="mt-1 flex flex-wrap items-center gap-2 text-[10px] text-text-muted">
                  <span>{sourceLabel(session.source)}</span>
                  <span>{session.project_path}</span>
                  {#if session.updated_at ?? session.timestamp}<span>{session.updated_at ?? session.timestamp}</span>{/if}
                </div>
              </div>
              <span class="shrink-0 font-mono text-[10px] text-text-muted">{formatSize(session.file_size_bytes)}</span>
            </div>
          </a>
        {/each}

        {#if recentSessions.length === 0}
          <div class="px-4 py-6 text-sm text-text-secondary">
            {loading || indexBuilding ? t("dash.loadingSessions") : t("dash.noSessions")}
          </div>
        {/if}
      </div>
    </section>

    <section class="rounded-lg border border-border bg-bg-secondary">
      <div class="border-b border-border px-4 py-3">
        <h2 class="text-sm font-semibold">{t("dash.topProjects")}</h2>
        <p class="mt-1 text-xs text-text-muted">{t("dash.topProjectsSub", { n: topProjects.length })}</p>
      </div>
      <div class="divide-y divide-border-subtle">
        {#each topProjects as p}
          <div class="flex items-center justify-between gap-4 px-4 py-2.5">
            <div class="min-w-0">
              <div class="truncate text-xs font-medium" title={p.path}>{p.path}</div>
              {#if p.lastActive}<div class="mt-0.5 text-[10px] text-text-muted">{p.lastActive}</div>{/if}
            </div>
            <span class="shrink-0 rounded-full bg-bg-tertiary px-2 py-0.5 text-[10px] text-text-muted">{t("dash.sessionsN", { n: p.count })}</span>
          </div>
        {/each}
        {#if topProjects.length === 0}
          <div class="px-4 py-6 text-sm text-text-secondary">{loading || indexBuilding ? t("dash.loading") : t("dash.noProjects")}</div>
        {/if}
      </div>
    </section>
  </main>
</div>
