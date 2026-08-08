<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import ConfirmDialog from "$lib/ConfirmDialog.svelte";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import { hostOfKey, withHostTag, withoutHostTag } from "$lib/hosts";
  import type { ProfileArchive, SourceInfo } from "$lib/types";

  let sources = $state<SourceInfo[]>([]);
  let profiles = $state<ProfileArchive[]>([]);
  let selectedSource = $state<string | null>(null);
  let loading = $state(true);
  let error = $state("");
  let showCreate = $state(false);
  let createName = $state("");
  /** Which machine the next snapshot is taken from; null means this one. */
  let createTarget = $state<string | null>(null);
  let confirmRestore = $state<string | null>(null);
  let confirmDelete = $state<string | null>(null);
  let busy = $state(false);

  async function refresh() {
    loading = true;
    try {
      sources = await api.sessions.listSources();
      if (!selectedSource) {
        selectedSource =
          sources.find((source) => source.available && source.capabilities.archive_read)?.id ?? null;
      }
      profiles = await api.profiles.list(selectedSource);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function archive() {
    if (!selectedSource || !canWriteArchive(selectedSource)) return;
    busy = true;
    try {
      // The host travels with the label: an untagged label snapshots the local install.
      await api.profiles.create(withHostTag(createName, createTarget) || undefined, selectedSource);
      showCreate = false;
      createName = "";
      createTarget = null;
      await refresh();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function restore(name: string) {
    const profile = profiles.find((profile) => profile.name === name);
    if (!canWriteArchive(profile?.source ?? selectedSource)) return;
    busy = true;
    try {
      await api.profiles.restore(name, profile?.source ?? selectedSource);
      confirmRestore = null;
      await refresh();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function del(name: string) {
    const profile = profiles.find((profile) => profile.name === name);
    if (!canWriteArchive(profile?.source ?? selectedSource)) return;
    busy = true;
    try {
      await api.profiles.delete(name, profile?.source ?? selectedSource);
      confirmDelete = null;
      await refresh();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function selectedSourceName(): string {
    return sources.find((source) => source.id === selectedSource)?.display_name ?? "";
  }

  function canWriteArchive(sourceId?: string | null): boolean {
    return sources.find((source) => source.id === sourceId)?.capabilities.archive_write ?? false;
  }

  function changeSource(source: string) {
    selectedSource = source;
    confirmRestore = null;
    confirmDelete = null;
    showCreate = false;
    refresh();
  }

  let archiveSources = $derived(sources.filter((source) => source.capabilities.archive_read));
  /** Snapshot targets for the selected source: this machine, plus any WSL install behind it. */
  let createTargets = $derived([
    null,
    ...(sources.find((source) => source.id === selectedSource)?.hosts ?? []),
  ]);
  let restoreTarget = $derived(profiles.find((profile) => profile.name === confirmRestore) ?? null);
  let deleteTarget = $derived(profiles.find((profile) => profile.name === confirmDelete) ?? null);

  onMount(() => deferRouteLoad(refresh));
</script>

<div class="p-8">
  <div class="flex items-center justify-between mb-8">
    <div>
      <h2 class="text-xl font-semibold tracking-tight">{selectedSourceName()} {t("profiles.snapshots")}</h2>
      <p class="text-xs text-text-muted mt-1">{t("profiles.subtitle")}</p>
      {#if selectedSource && !canWriteArchive(selectedSource)}
        <div class="mt-2 inline-flex rounded-md bg-bg-tertiary px-2 py-1 text-[10px] text-text-muted">{t("common.readonly")}</div>
      {/if}
      {#if archiveSources.length > 1}
        <div class="mt-3 inline-flex rounded-lg border border-border overflow-hidden">
          {#each archiveSources as source}
            <button
              onclick={() => changeSource(source.id)}
              disabled={!source.available}
              class="px-2.5 py-1 text-[10px] transition-colors
                {selectedSource === source.id ? 'bg-accent text-white' : source.available ? 'hover:bg-bg-hover text-text-secondary' : 'text-text-muted opacity-50'}"
            >
              {source.display_name}
            </button>
          {/each}
        </div>
      {/if}
    </div>
    <button
      onclick={() => (showCreate = true)}
      disabled={!selectedSource || !canWriteArchive(selectedSource)}
      class="px-4 py-2 text-xs font-medium bg-accent text-white rounded-lg
        hover:bg-accent-hover transition-all shadow-sm shadow-accent/20 disabled:opacity-40"
    >
      {t("profiles.create")}
    </button>
  </div>

  {#if error}
    <div class="mb-6 p-4 bg-danger-dim border border-danger/20 rounded-xl text-sm text-danger">
      {error}
      <button class="ml-3 underline opacity-70 hover:opacity-100" onclick={() => (error = "")}>{t("profiles.dismiss")}</button>
    </div>
  {/if}

  {#if showCreate}
    <div class="mb-6 bg-bg-secondary border border-border rounded-xl p-5">
      <h3 class="text-sm font-medium mb-3">{t("profiles.newTitle")}</h3>
      {#if createTargets.length > 1}
        <div class="mb-3 flex items-center gap-2">
          <span class="text-[10px] text-text-secondary">{t("prof.target")}</span>
          <div class="inline-flex overflow-hidden rounded-lg border border-border">
            {#each createTargets as target}
              <button
                onclick={() => (createTarget = target)}
                class="px-2.5 py-1 text-[10px] transition-colors
                  {createTarget === target ? 'bg-accent text-white' : 'text-text-secondary hover:bg-bg-hover'}"
              >
                {target ?? t("prof.targetNative")}
              </button>
            {/each}
          </div>
        </div>
      {/if}
      <div class="flex gap-3">
        <input
          bind:value={createName}
          placeholder={t("profiles.namePlaceholder")}
          class="flex-1 px-3 py-2 text-sm bg-bg border border-border rounded-lg
            focus:border-accent focus:ring-1 focus:ring-accent/20 outline-none transition-all"
        />
        <button onclick={archive} disabled={busy}
          class="px-5 py-2 text-xs font-medium bg-accent text-white rounded-lg
            hover:bg-accent-hover disabled:opacity-40 transition-all">
          {busy ? t("profiles.creating") : t("profiles.createBtn")}
        </button>
        <button onclick={() => (showCreate = false)}
          class="px-4 py-2 text-xs border border-border rounded-lg hover:bg-bg-hover transition-all">
          {t("common.cancel")}
        </button>
      </div>
    </div>
  {/if}

  {#if loading}
    <div class="flex items-center justify-center h-40 text-text-muted text-sm">{t("common.loading")}</div>
  {:else if profiles.length === 0}
    <div class="flex flex-col items-center justify-center h-40 text-text-muted">
      <p class="text-sm">{t("profiles.empty")}</p>
      <p class="text-xs mt-1">{t("profiles.emptyHint")}</p>
    </div>
  {:else}
    <div class="space-y-2">
      {#each profiles as p}
        <div class="bg-bg-secondary border border-border rounded-xl p-5 flex items-center justify-between
          hover:border-border-hover transition-colors">
          <div class="min-w-0 flex-1">
            <div class="flex items-center gap-2">
              <span class="text-sm font-medium truncate">{withoutHostTag(p.name)}</span>
              {#if hostOfKey(p.name)}
                <span class="rounded-full bg-bg-tertiary px-1.5 py-0.5 text-[10px] text-text-muted">{hostOfKey(p.name)}</span>
              {/if}
              {#if p.is_auto}
                <span class="text-[10px] px-1.5 py-0.5 rounded-full bg-bg-tertiary text-text-muted">{t("profiles.auto")}</span>
              {/if}
            </div>
            <div class="text-xs text-text-muted mt-1 flex items-center gap-3">
              <span>{p.created}</span>
              <span class="w-1 h-1 rounded-full bg-border"></span>
              <span>{p.size_human}</span>
              <span class="w-1 h-1 rounded-full bg-border"></span>
              <span>{t("profiles.itemsN", { n: p.items })}</span>
              {#if p.note}
                <span class="w-1 h-1 rounded-full bg-border"></span>
                <span class="italic">{p.note}</span>
              {/if}
            </div>
          </div>
          <div class="flex gap-2 ml-4 shrink-0">
            {#if !canWriteArchive(p.source)}
              <span class="self-center rounded-md bg-bg-tertiary px-2 py-1 text-[10px] text-text-muted">{t("common.readonly")}</span>
            {:else}
              <button onclick={() => { confirmDelete = null; confirmRestore = p.name; }}
                class="px-3 py-1.5 text-xs border border-border rounded-lg hover:bg-bg-hover
                  hover:border-accent/40 transition-all">{t("profiles.restore")}</button>
              <button onclick={() => { confirmRestore = null; confirmDelete = p.name; }}
                class="px-3 py-1.5 text-xs border border-danger/30 text-danger rounded-lg
                  hover:bg-danger-dim transition-all">{t("profiles.delete")}</button>
            {/if}
          </div>
        </div>
      {/each}
    </div>
  {/if}

  <ConfirmDialog
    open={restoreTarget !== null}
    tone="warning"
    title={t("profiles.restoreTitle")}
    message={restoreTarget
      ? t("profiles.restoreMsg", { name: restoreTarget.name, src: restoreTarget.source_display_name })
      : ""}
    detail={restoreTarget ? `${restoreTarget.name} · ${restoreTarget.created} · ${restoreTarget.size_human}` : null}
    confirmLabel={t("profiles.restore")}
    {busy}
    onconfirm={() => confirmRestore && restore(confirmRestore)}
    oncancel={() => (confirmRestore = null)}
  />

  <ConfirmDialog
    open={deleteTarget !== null}
    tone="danger"
    title={t("profiles.deleteTitle")}
    message={deleteTarget ? t("profiles.deleteMsg", { name: deleteTarget.name }) : ""}
    detail={deleteTarget ? `${deleteTarget.name} · ${deleteTarget.created} · ${deleteTarget.size_human}` : null}
    confirmLabel={t("profiles.delete")}
    {busy}
    onconfirm={() => confirmDelete && del(confirmDelete)}
    oncancel={() => (confirmDelete = null)}
  />
</div>
