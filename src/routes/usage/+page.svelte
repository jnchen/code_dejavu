<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import { getLang, t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import { priceForIn } from "$lib/prices";
  import type { UsageSummary, UsageBucket, PriceRow } from "$lib/types";

  let usage = $state<UsageSummary | null>(null);
  let prices = $state<PriceRow[]>([]);
  let loading = $state(true);
  let error = $state("");
  let activeDay = $state<UsageBucket | null>(null);

  async function load() {
    loading = true;
    error = "";
    try {
      const [u, cfg] = await Promise.all([api.sessions.usageSummary(), api.dejavu.getConfig()]);
      usage = u;
      prices = cfg.prices ?? [];
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function fmt(n: number): string {
    if (n >= 1e9) return (n / 1e9).toFixed(2) + "B";
    if (n >= 1e6) return (n / 1e6).toFixed(2) + "M";
    if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
    return String(n);
  }

  function fmtExact(n: number): string {
    return new Intl.NumberFormat(getLang() === "zh" ? "zh-CN" : "en-US").format(n);
  }

  function barMax(buckets: UsageBucket[]): number {
    return Math.max(1, ...buckets.map((b) => b.total_tokens));
  }

  // Cost is an ESTIMATE — prices (editable in Settings, stored in the app config) change and
  // cache is blended — so it's labelled as a rough guide, not a bill.
  function costOf(b: UsageBucket): number | null {
    const p = priceForIn(prices, b.key);
    if (!p) return null;
    // Cache priced at ~0.1x input (mostly cache reads) — rough.
    return (b.input_tokens / 1e6) * p.input + (b.cache_tokens / 1e6) * p.input * 0.1 + (b.output_tokens / 1e6) * p.output;
  }
  function fmtUsd(n: number): string {
    if (n >= 100) return "$" + n.toFixed(0);
    if (n >= 1) return "$" + n.toFixed(2);
    return "$" + n.toFixed(3);
  }
  let totalCost = $derived.by(() => {
    let cost = 0;
    let hasUnknown = false;
    for (const b of usage?.by_model ?? []) {
      const c = costOf(b);
      if (c == null) {
        if (b.total_tokens > 0) hasUnknown = true;
      } else {
        cost += c;
      }
    }
    return { cost, hasUnknown };
  });

  onMount(() => deferRouteLoad(load));
</script>

<div class="p-6 h-full flex flex-col">
  <div class="mb-4 flex items-center gap-3 shrink-0">
    <h2 class="text-lg font-semibold shrink-0">{t("usage.title")}</h2>
    <p class="flex-1 text-xs text-text-muted">{t("usage.subtitle")}</p>
    <button
      onclick={load}
      disabled={loading}
      class="shrink-0 rounded-lg border border-border px-2.5 py-1.5 text-[11px] text-text-secondary hover:bg-bg-hover transition-colors disabled:opacity-50"
    >{loading ? t("usage.scanning") : t("common.refresh")}</button>
  </div>

  {#if error}
    <div class="mb-4 p-3 bg-danger-dim border border-danger/30 rounded-xl text-sm text-danger shrink-0">{error}</div>
  {/if}

  {#if loading}
    <p class="text-text-secondary text-sm">{t("usage.computing")}</p>
  {:else if usage}
    <div class="flex-1 overflow-y-auto space-y-6">
      <!-- Totals -->
      <section class="grid grid-cols-2 gap-3 md:grid-cols-5">
        {#each [[t("usage.sessions"), usage.totals.sessions], [t("usage.totalTokens"), usage.totals.total_tokens], [t("usage.input"), usage.totals.input_tokens], [t("usage.output"), usage.totals.output_tokens], [t("usage.cache"), usage.totals.cache_tokens]] as [label, val]}
          <div class="rounded-xl border border-border bg-bg-secondary p-4">
            <div class="text-[10px] uppercase tracking-wider text-text-muted">{label}</div>
            <div class="mt-1 text-xl font-semibold tabular-nums">{fmt(val as number)}</div>
          </div>
        {/each}
      </section>

      {#if usage.totals.total_tokens === 0}
        <div class="rounded-xl border border-border bg-bg-secondary p-4 text-xs text-text-secondary">
          {t("usage.noData")}
        </div>
      {:else}
        <div class="rounded-xl border border-border bg-bg-secondary p-4">
          <div class="flex items-baseline gap-2 flex-wrap">
            <span class="text-[10px] uppercase tracking-wider text-text-muted">{t("usage.estCost")}</span>
            <span class="text-xl font-semibold tabular-nums">{fmtUsd(totalCost.cost)}</span>
            {#if totalCost.hasUnknown}<span class="text-[10px] text-text-muted">{t("usage.someUnpriced")}</span>{/if}
          </div>
          <p class="mt-1 text-[10px] text-text-muted">{t("usage.costNote")}</p>
        </div>
      {/if}

      <div class="grid gap-6 md:grid-cols-2">
        <!-- By source -->
        <section>
          <h3 class="mb-2 text-sm font-semibold">{t("usage.bySource")}</h3>
          {#each usage.by_source as b}
            {@const max = barMax(usage.by_source)}
            <div class="mb-2">
              <div class="flex items-center justify-between text-[11px]">
                <span class="truncate">{b.key}</span>
                <span class="shrink-0 text-text-muted tabular-nums">{fmt(b.total_tokens)} · {b.sessions} {t("common.sessionsSuffix")}</span>
              </div>
              <div class="mt-1 h-2 rounded-full bg-bg-tertiary overflow-hidden">
                <div class="h-full rounded-full bg-accent" style="width: {Math.max(2, (b.total_tokens / max) * 100)}%"></div>
              </div>
            </div>
          {/each}
          {#if usage.by_source.length === 0}<p class="text-xs text-text-secondary">{t("usage.empty")}</p>{/if}
        </section>

        <!-- By model -->
        <section>
          <h3 class="mb-2 text-sm font-semibold">{t("usage.byModel")}</h3>
          {#each usage.by_model as b}
            {@const max = barMax(usage.by_model)}
            {@const cost = costOf(b)}
            <div class="mb-2">
              <div class="flex items-center justify-between text-[11px]">
                <span class="truncate" title={b.key}>{b.key}</span>
                <span class="shrink-0 text-text-muted tabular-nums">{fmt(b.total_tokens)}{#if cost != null} · {fmtUsd(cost)}{/if} · {b.sessions} {t("common.sessionsSuffix")}</span>
              </div>
              <div class="mt-1 h-2 rounded-full bg-bg-tertiary overflow-hidden">
                <div class="h-full rounded-full bg-accent" style="width: {Math.max(2, (b.total_tokens / max) * 100)}%"></div>
              </div>
            </div>
          {/each}
          {#if usage.by_model.length === 0}<p class="text-xs text-text-secondary">{t("usage.empty")}</p>{/if}
        </section>
      </div>

      <!-- By project -->
      <section>
        <h3 class="mb-2 text-sm font-semibold">{t("usage.byProject", { n: usage.by_project.length })}</h3>
        {#each usage.by_project as b}
          {@const max = barMax(usage.by_project)}
          <div class="mb-2">
            <div class="flex items-center justify-between text-[11px]">
              <span class="truncate" title={b.key}>{b.key}</span>
              <span class="shrink-0 text-text-muted tabular-nums">{fmt(b.total_tokens)} · {b.sessions} {t("common.sessionsSuffix")}</span>
            </div>
            <div class="mt-1 h-2 rounded-full bg-bg-tertiary overflow-hidden">
              <div class="h-full rounded-full bg-accent" style="width: {Math.max(2, (b.total_tokens / max) * 100)}%"></div>
            </div>
          </div>
        {/each}
        {#if usage.by_project.length === 0}<p class="text-xs text-text-secondary">{t("usage.empty")}</p>{/if}
      </section>

      <!-- By day -->
      {#if usage.by_day.length > 0}
        {@const dayMax = barMax(usage.by_day)}
        <section>
          <div class="mb-2 flex items-baseline gap-2">
            <h3 class="text-sm font-semibold">{t("usage.byDay")}</h3>
            <span class="text-[10px] text-text-muted">{t("usage.dayHoverHint")}</span>
          </div>
          <div class="relative h-40">
            {#if activeDay}
              <div class="pointer-events-none absolute inset-x-0 top-0 z-20 flex justify-center">
                <div class="min-w-36 max-w-full rounded-lg border border-border bg-bg-secondary px-3 py-2 text-left shadow-lg">
                  <div class="mb-1 whitespace-nowrap text-[11px] font-medium text-text">{activeDay.key}</div>
                  <div class="flex items-center justify-between gap-3 whitespace-nowrap text-[10px]">
                    <span class="text-text-muted">{t("usage.totalTokens")}</span>
                    <span class="font-semibold tabular-nums text-text">{fmtExact(activeDay.total_tokens)}</span>
                  </div>
                  <div class="flex items-center justify-between gap-3 whitespace-nowrap text-[10px]">
                    <span class="text-text-muted">{t("usage.sessions")}</span>
                    <span class="tabular-nums text-text-secondary">{fmtExact(activeDay.sessions)}</span>
                  </div>
                </div>
              </div>
            {/if}
            <div class="absolute inset-x-0 bottom-0 flex h-[88px] items-end gap-[2px]">
              {#each usage.by_day as d}
                {@const barHeight = Math.max(2, Math.round((d.total_tokens / dayMax) * 88))}
                <button
                  type="button"
                  class="group relative h-full min-w-0 flex-1 cursor-help border-0 bg-transparent p-0 focus:outline-none"
                  aria-label={t("usage.dayAria", {
                    date: d.key,
                    tokens: fmtExact(d.total_tokens),
                    sessions: d.sessions
                  })}
                  onmouseenter={() => activeDay = d}
                  onmouseleave={() => activeDay = null}
                  onfocus={() => activeDay = d}
                  onblur={() => activeDay = null}
                >
                  <div class="absolute bottom-0 left-0 w-full rounded-t-sm bg-accent/60 transition-colors group-hover:bg-accent group-focus-visible:bg-accent"
                    style="height: {barHeight}px"></div>
                </button>
              {/each}
            </div>
          </div>
        </section>
      {/if}
    </div>
  {/if}
</div>
