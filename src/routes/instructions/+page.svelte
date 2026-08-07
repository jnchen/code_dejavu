<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import Markdown from "$lib/Markdown.svelte";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import type { InstructionArtifact, InstructionDetail, SourceInfo } from "$lib/types";

  let sources = $state<SourceInfo[]>([]);
  let artifacts = $state<InstructionArtifact[]>([]);
  let selected = $state<InstructionArtifact | null>(null);
  let detail = $state<InstructionDetail | null>(null);
  let content = $state("");
  let loading = $state(true);
  let loadingDetail = $state(false);
  let saving = $state(false);
  let saved = $state(false);
  let error = $state("");
  let editing = $state(false);

  let lineCount = $derived(content ? content.split("\n").length : 0);
  let charCount = $derived(content.length);
  let instructionSources = $derived(
    sources.filter((source) => source.capabilities.instructions_read && artifactsFor(source.id).length > 0)
  );

  async function refresh() {
    loading = true;
    error = "";
    try {
      const [sourceList, artifactList] = await Promise.all([
        api.sessions.listSources(),
        api.instructions.list(),
      ]);
      const instructionArtifacts = artifactList.filter(
        (artifact) => artifact.kind === "instructions" && artifact.scope !== "project"
      );
      sources = sourceList;
      artifacts = instructionArtifacts;

      const current = selected
        ? instructionArtifacts.find((artifact) => artifact.source === selected?.source && artifact.path === selected?.path)
        : null;
      const next = current ?? instructionArtifacts[0] ?? null;
      if (next) await openArtifact(next);
      else {
        selected = null;
        detail = null;
        content = "";
      }
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function openArtifact(artifact: InstructionArtifact) {
    selected = artifact;
    editing = false;
    saved = false;
    loadingDetail = true;
    error = "";
    try {
      detail = await api.instructions.get(artifact.source, artifact.path);
      selected = detail;
      content = detail.content;
    } catch (e) {
      error = String(e);
    } finally {
      loadingDetail = false;
    }
  }

  async function save() {
    if (!selected?.editable) return;
    saving = true;
    saved = false;
    error = "";
    try {
      await api.instructions.save(selected.source, selected.path, content);
      saved = true;
      editing = false;
      await refresh();
      setTimeout(() => (saved = false), 2000);
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  function artifactsFor(sourceId: string): InstructionArtifact[] {
    return artifacts.filter((artifact) => artifact.source === sourceId);
  }

  function isSelected(artifact: InstructionArtifact): boolean {
    return selected?.source === artifact.source && selected?.path === artifact.path;
  }

  function formatSize(bytes: number): string {
    if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
  }

  function sourceState(source: SourceInfo): string {
    if (!source.available) return t("common.notFound");
    if (source.capabilities.instructions_write) return t("common.editable");
    return t("common.readonly");
  }

  function sourceStateClass(source: SourceInfo): string {
    if (!source.available) return "bg-bg-tertiary text-text-muted";
    if (source.capabilities.instructions_write) return "bg-success-dim text-success";
    return "bg-accent-dim text-accent";
  }

  onMount(() => deferRouteLoad(refresh));
</script>

<div class="flex h-full">
  <aside class="w-80 shrink-0 overflow-y-auto border-r border-border bg-bg-secondary">
    <div class="border-b border-border px-4 py-3">
      <h2 class="text-sm font-semibold">{t("inst.title")}</h2>
      <p class="mt-1 text-xs text-text-muted">{t("inst.sub")}</p>
    </div>

    {#if loading}
      <p class="px-4 py-4 text-sm text-text-secondary">{t("common.loading")}</p>
    {:else}
      <div class="divide-y divide-border-subtle">
        {#if instructionSources.length === 0}
          <p class="px-4 py-4 text-sm text-text-secondary">{t("inst.noFiles")}</p>
        {/if}
        {#each instructionSources as source}
          {@const sourceArtifacts = artifactsFor(source.id)}
          <section class="px-3 py-3">
            <div class="mb-2 flex items-center justify-between gap-2">
              <div class="min-w-0">
                <div class="truncate text-xs font-medium">{source.display_name}</div>
                <div class="mt-0.5 text-[10px] text-text-muted">
                  {t("inst.globalFilesN", { n: sourceArtifacts.length })}
                </div>
              </div>
              <span class="shrink-0 rounded-md px-1.5 py-0.5 text-[10px] font-medium {sourceStateClass(source)}">
                {sourceState(source)}
              </span>
            </div>

            {#if sourceArtifacts.length > 0}
              <div class="space-y-1">
                {#each sourceArtifacts as artifact}
                  <button
                    onclick={() => openArtifact(artifact)}
                    class="w-full rounded-lg px-2.5 py-2 text-left transition-colors
                      {isSelected(artifact) ? 'bg-accent-dim text-accent' : 'hover:bg-bg-hover text-text-secondary'}"
                  >
                    <div class="flex items-center justify-between gap-2">
                      <span class="truncate text-xs font-medium">{artifact.title}</span>
                      <span class="shrink-0 rounded bg-bg px-1.5 py-0.5 text-[9px] text-text-muted">{t("common.global")}</span>
                    </div>
                    <div class="mt-0.5 truncate font-mono text-[10px] text-text-muted">{artifact.path}</div>
                  </button>
                {/each}
              </div>
            {:else}
              <div class="rounded-lg bg-bg px-3 py-2 text-[11px] text-text-muted">
                {t("inst.noFilesShort")}
              </div>
            {/if}
          </section>
        {/each}
      </div>
    {/if}
  </aside>

  <section class="flex min-w-0 flex-1 flex-col">
    <div class="flex items-center justify-between gap-4 border-b border-border px-6 py-3">
      <div class="min-w-0">
        <div class="flex items-center gap-3">
          <h1 class="truncate text-lg font-semibold">{selected?.title ?? t("inst.title")}</h1>
          {#if selected}
            <span class="rounded-md px-1.5 py-0.5 text-[10px] {selected.editable ? 'bg-success-dim text-success' : 'bg-bg-tertiary text-text-muted'}">
              {selected.editable ? t("common.editable") : t("common.readonly")}
            </span>
            {#if !selected.exists}
              <span class="rounded-md bg-warning-dim px-1.5 py-0.5 text-[10px] text-warning">{t("common.notCreated")}</span>
            {/if}
          {/if}
        </div>
        <div class="mt-0.5 truncate font-mono text-[10px] text-text-muted">
          {selected?.path ?? t("inst.selectHint")}
        </div>
      </div>

      <div class="flex shrink-0 items-center gap-2">
        {#if selected}
          <span class="text-xs text-text-secondary">{t("inst.linesN", { n: lineCount })} · {t("inst.charsN", { n: charCount })} · {formatSize(selected.size_bytes)}</span>
        {/if}
        {#if selected?.editable}
          {#if editing}
            <button
              onclick={save}
              disabled={saving}
              class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50"
            >
              {saving ? t("common.saving") : t("common.save")}
            </button>
            <button
              onclick={() => selected && openArtifact(selected)}
              class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover"
            >
              {t("common.cancel")}
            </button>
          {:else}
            <button
              onclick={() => (editing = true)}
              class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover"
            >
              {t("common.edit")}
            </button>
          {/if}
        {/if}
      </div>
    </div>

    {#if error}
      <div class="mx-6 mt-3 rounded-lg border border-danger/30 bg-danger-dim p-3 text-sm text-danger">{error}</div>
    {/if}

    {#if saved}
      <div class="mx-6 mt-3 rounded-lg border border-success/30 bg-success-dim p-3 text-sm text-success">{t("common.saved")}</div>
    {/if}

    {#if loadingDetail}
      <p class="p-6 text-sm text-text-secondary">{t("common.loading")}</p>
    {:else if !selected}
      <div class="flex flex-1 items-center justify-center text-sm text-text-secondary">
        {t("inst.noViewable")}
      </div>
    {:else if editing}
      <textarea
        bind:value={content}
        class="flex-1 resize-none bg-bg px-6 py-4 font-mono text-xs leading-relaxed focus:outline-none"
        spellcheck="false"
      ></textarea>
    {:else}
      <div class="flex-1 overflow-y-auto px-6 py-4">
        {#if detail?.description}
          <p class="mb-4 max-w-3xl text-xs text-text-secondary">{detail.description}</p>
        {/if}
        {#if content}
          <Markdown {content} />
        {:else}
          <div class="rounded-lg border border-border bg-bg-secondary px-4 py-6 text-sm text-text-muted">
            {selected.editable ? t("inst.emptyEditable") : t("inst.emptyReadonly")}
          </div>
        {/if}
      </div>
    {/if}
  </section>
</div>
