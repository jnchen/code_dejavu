<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "$lib/api";
  import Markdown from "$lib/Markdown.svelte";
  import ConfirmDialog from "$lib/ConfirmDialog.svelte";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import type { ProjectInfo, MemoryFile, MemoryFrontmatter, SourceInfo } from "$lib/types";

  let sources = $state<SourceInfo[]>([]);
  let projects = $state<ProjectInfo[]>([]);
  let selectedSource = $state<string | null>(null);
  let selectedProject = $state<string | null>(null);
  let memories = $state<MemoryFile[]>([]);
  let viewing = $state<MemoryFile | null>(null);
  let editing = $state<MemoryFile | null>(null);
  let editContent = $state("");
  let editName = $state("");
  let editDesc = $state("");
  let editType = $state("feedback");
  let loading = $state(true);
  let error = $state("");
  let saving = $state(false);
  let deleting = $state(false);
  let searchQuery = $state("");
  let filterType = $state<string | null>(null);
  let confirmDelete = $state<string | null>(null);
  let deleteTarget = $derived(memories.find((m) => m.filename === confirmDelete) ?? null);

  const TYPE_KEYS = ["feedback", "project", "user", "reference", "index", "thread"];
  function typeLabel(type: string): string {
    return TYPE_KEYS.includes(type) ? t("mem.type." + type) : type;
  }

  function memoryIdentity(memory: MemoryFile, type: string): string {
    if (type !== "thread") return memory.filename;
    return `${t("mem.sessionId")} ${memory.filename.replace(/\.md$/i, "").slice(0, 8)}`;
  }

  const typeColors: Record<string, string> = {
    feedback: "bg-warning-dim text-warning",
    project: "bg-accent-dim text-accent",
    user: "bg-success-dim text-success",
    reference: "bg-bg-tertiary text-text-muted",
    index: "bg-danger-dim text-danger",
    thread: "bg-accent-dim text-accent",
  };

  let filteredMemories = $derived(
    memories.filter((m) => {
      if (filterType && (m.frontmatter?.type ?? "unknown") !== filterType) return false;
      if (searchQuery) {
        const q = searchQuery.toLowerCase();
        const name = (m.frontmatter?.name ?? m.filename).toLowerCase();
        const desc = (m.frontmatter?.description ?? "").toLowerCase();
        return name.includes(q) || desc.includes(q) || m.filename.toLowerCase().includes(q);
      }
      return true;
    })
  );

  let typeCounts = $derived(() => {
    const counts: Record<string, number> = {};
    for (const m of memories) {
      const t = (m.frontmatter?.type ?? "unknown");
      counts[t] = (counts[t] ?? 0) + 1;
    }
    return counts;
  });

  async function loadProjects() {
    loading = true;
    try {
      sources = await api.sessions.listSources();
      if (!selectedSource) {
        selectedSource =
          sources.find((source) => source.available && source.capabilities.memory_read)?.id ?? null;
      }
      projects = await api.memories.listProjects(selectedSource);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function selectProject(slug: string) {
    selectedProject = slug;
    viewing = null;
    editing = null;
    searchQuery = "";
    filterType = null;
    confirmDelete = null;
    try {
      memories = await api.memories.list(slug, selectedSource);
    } catch (e) {
      error = String(e);
    }
  }

  function viewMemory(m: MemoryFile) {
    viewing = m;
    editing = null;
  }

  // Local links inside MEMORY.md (e.g. `[Title](some-memory.md)`) navigate to that
  // sibling memory file inside the app instead of opening a browser.
  function navigateToMemory(href: string) {
    const target = decodeURIComponent(href.replace(/^\.\//, "").split(/[?#]/)[0]);
    const base = target.replace(/\.md$/i, "");
    const found = memories.find(
      (m) => m.filename === target || m.filename === base + ".md" || m.frontmatter?.name === base
    );
    if (found) viewMemory(found);
    else error = t("mem.linkNotFound", { href });
  }

  function startEditing(m: MemoryFile) {
    if (!canWriteMemory(m.source)) return;
    editing = m;
    editContent = m.content;
    editName = m.frontmatter?.name ?? "";
    editDesc = m.frontmatter?.description ?? "";
    editType = m.frontmatter?.type ?? "feedback";
  }

  async function saveMemory() {
    if (!editing || !selectedProject || !canWriteMemory(editing.source)) return;
    saving = true;
    try {
      const fm: MemoryFrontmatter = {
        name: editName || null,
        description: editDesc || null,
        type: editType || null,
        metadata: null,
      };
      await api.memories.save(selectedProject, editing.filename, fm, editContent, editing.source);
      memories = await api.memories.list(selectedProject, editing.source);
      const updated = memories.find(m => m.filename === editing!.filename);
      editing = null;
      if (updated) viewing = updated; else viewing = null;
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  async function doDelete(m: MemoryFile) {
    if (!selectedProject || !canWriteMemory(m.source)) return;
    deleting = true;
    try {
      await api.memories.delete(selectedProject, m.filename, m.source);
      memories = await api.memories.list(selectedProject, m.source);
      if (editing?.filename === m.filename) editing = null;
      confirmDelete = null;
    } catch (e) {
      error = String(e);
    } finally {
      deleting = false;
    }
  }

  function selectedSourceName(): string {
    return sources.find((source) => source.id === selectedSource)?.display_name ?? "";
  }

  function canWriteMemory(sourceId?: string | null): boolean {
    return sources.find((source) => source.id === sourceId)?.capabilities.memory_write ?? false;
  }

  function changeSource(source: string) {
    selectedSource = source;
    selectedProject = null;
    memories = [];
    viewing = null;
    editing = null;
    loadProjects();
  }

  let memorySources = $derived(sources.filter((source) => source.capabilities.memory_read));

  onMount(() => deferRouteLoad(loadProjects));
</script>

<div class="flex h-full">
  <!-- Project sidebar -->
  <div class="w-72 shrink-0 border-r border-border overflow-y-auto">
    <div class="p-3 border-b border-border">
      <h3 class="text-xs font-medium text-text-secondary uppercase tracking-wider">{t("mem.projectsOf", { name: selectedSourceName() })}</h3>
      {#if memorySources.length > 1}
        <div class="mt-2 flex rounded-lg border border-border overflow-hidden">
          {#each memorySources as source}
            <button
              onclick={() => changeSource(source.id)}
              disabled={!source.available}
              class="flex-1 px-2 py-1 text-[10px] transition-colors
                {selectedSource === source.id ? 'bg-accent text-white' : source.available ? 'hover:bg-bg-hover text-text-secondary' : 'text-text-muted opacity-50'}"
            >
              {source.display_name}
            </button>
          {/each}
        </div>
      {/if}
    </div>
    {#if loading}
      <p class="p-3 text-xs text-text-secondary">{t("common.loading")}</p>
    {:else}
      {#each projects as p}
        <button
          onclick={() => selectProject(p.slug)}
          class="w-full text-left px-3 py-2.5 border-b border-border-subtle hover:bg-bg-hover transition-colors
            {selectedProject === p.slug ? 'bg-bg-tertiary' : ''}"
        >
          <div class="text-xs font-medium {selectedProject === p.slug ? 'text-accent' : 'text-text'}" title={p.display_path}>
            {p.display_path}
          </div>
          <div class="text-[10px] text-text-muted mt-0.5 flex items-center gap-2 flex-wrap">
            <span>{t("mem.memoriesN", { n: p.memory_count })}</span>
            <span class="w-1 h-1 rounded-full bg-border"></span>
            <span>{t("mem.sessionsN", { n: p.session_count })}</span>
            {#if p.last_active}
              <span class="w-1 h-1 rounded-full bg-border"></span>
              <span>{p.last_active}</span>
            {/if}
          </div>
        </button>
      {/each}
    {/if}
  </div>

  <!-- Main content -->
  <div class="flex-1 overflow-y-auto">
    {#if error}
      <div class="m-4 p-3 bg-danger-dim border border-danger/30 rounded-xl text-sm text-danger">{error}</div>
    {/if}

    {#if viewing && !editing}
      <!-- View mode -->
      <div class="p-5">
        <div class="flex items-center justify-between mb-4">
          <div>
            <h3 class="text-sm font-medium">{viewing.frontmatter?.name ?? viewing.filename}</h3>
            <div class="text-[10px] text-text-muted mt-0.5 flex items-center gap-2">
              <span>{viewing.filename}</span>
              {#if viewing.frontmatter?.type}
                <span class="w-1 h-1 rounded-full bg-border"></span>
                <span>{viewing.frontmatter.type}</span>
              {/if}
            </div>
            {#if viewing.frontmatter?.description}
              <p class="text-xs text-text-secondary mt-1">{viewing.frontmatter.description}</p>
            {/if}
          </div>
          {#if canWriteMemory(viewing.source)}
            <button onclick={() => viewing && startEditing(viewing)}
              class="px-3 py-1.5 text-xs border border-border rounded-lg hover:bg-bg-hover">
              {t("common.edit")}
            </button>
          {:else}
            <span class="rounded-md bg-bg-tertiary px-2 py-1 text-[10px] text-text-muted">{t("common.readonly")}</span>
          {/if}
        </div>
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div class="bg-bg-secondary border border-border rounded-xl p-5 overflow-y-auto"
          onclick={(e) => {
            const target = (e.target as HTMLElement).closest('a');
            if (!target) return;
            const href = target.getAttribute('href');
            if (!href) return;
            e.preventDefault();
            const filename = href.replace(/^\.\//, '');
            if (filename.endsWith('.md')) {
              const found = memories.find(m => m.filename === filename);
              if (found) { viewMemory(found); return; }
            }
          }}
        >
          <Markdown content={viewing.content} onLocalLink={navigateToMemory} />
        </div>
      </div>
    {:else if editing}
      <!-- Editor -->
      <div class="p-5 space-y-4">
        <div class="flex items-center justify-between">
          <div>
            <h3 class="text-sm font-medium">{editing.filename}</h3>
            <p class="text-[10px] text-text-muted mt-0.5">{editing.project_path}</p>
          </div>
          <div class="flex gap-2">
            <button onclick={saveMemory} disabled={saving}
              class="px-4 py-1.5 text-xs bg-accent text-white rounded-lg hover:bg-accent-hover disabled:opacity-50">
              {saving ? t("common.saving") : t("common.save")}
            </button>
            <button onclick={() => { editing = null; }}
              class="px-3 py-1.5 text-xs border border-border rounded-lg hover:bg-bg-hover">{t("common.cancel")}</button>
          </div>
        </div>

        <div class="bg-bg-secondary border border-border rounded-xl p-4 space-y-3">
          <h4 class="text-[10px] text-text-muted uppercase tracking-wider">{t("mem.metadata")}</h4>
          <div class="grid grid-cols-3 gap-3">
            <div>
              <label for="edit-name" class="block text-[10px] text-text-muted mb-1">{t("mem.fieldName")}</label>
              <input id="edit-name" bind:value={editName} placeholder="memory name"
                class="w-full px-2.5 py-1.5 text-xs bg-bg border border-border rounded-lg focus:border-accent outline-none" />
            </div>
            <div>
              <label for="edit-desc" class="block text-[10px] text-text-muted mb-1">{t("mem.fieldDesc")}</label>
              <input id="edit-desc" bind:value={editDesc} placeholder="brief description"
                class="w-full px-2.5 py-1.5 text-xs bg-bg border border-border rounded-lg focus:border-accent outline-none" />
            </div>
            <div>
              <label for="edit-type" class="block text-[10px] text-text-muted mb-1">{t("mem.fieldType")}</label>
              <select id="edit-type" bind:value={editType}
                class="w-full px-2.5 py-1.5 text-xs bg-bg border border-border rounded-lg focus:border-accent outline-none">
                <option value="feedback">feedback - {t("mem.type.feedback")}</option>
                <option value="project">project - {t("mem.type.project")}</option>
                <option value="user">user - {t("mem.type.user")}</option>
                <option value="reference">reference - {t("mem.type.reference")}</option>
              </select>
            </div>
          </div>
        </div>

        <textarea bind:value={editContent}
          class="w-full h-80 px-4 py-3 text-xs font-mono bg-bg-secondary border border-border rounded-xl resize-none focus:border-accent outline-none leading-relaxed"
          spellcheck="false"
        ></textarea>
      </div>
    {:else if selectedProject}
      <!-- Memory list -->
      <div class="p-4">
        <div class="flex items-center gap-3 mb-4">
          <h3 class="text-sm font-medium">{t("mem.title")}（{filteredMemories.length}{filterType || searchQuery ? ` / ${memories.length}` : ''}）</h3>
          <input
            bind:value={searchQuery}
            placeholder={t("mem.searchPlaceholder")}
            class="px-2.5 py-1 text-xs bg-bg-secondary border border-border rounded-lg focus:border-accent outline-none w-40"
          />
          <div class="flex gap-1 ml-auto">
            <button
              onclick={() => (filterType = null)}
              class="text-[10px] px-2 py-1 rounded-lg transition-colors
                {filterType === null ? 'bg-accent text-white' : 'bg-bg-secondary hover:bg-bg-hover text-text-secondary'}"
            >{t("mem.all")}</button>
            {#each TYPE_KEYS as type}
              <button
                onclick={() => (filterType = filterType === type ? null : type)}
                class="text-[10px] px-2 py-1 rounded-lg transition-colors
                  {filterType === type ? 'bg-accent text-white' : 'bg-bg-secondary hover:bg-bg-hover text-text-secondary'}"
              >{typeLabel(type)}</button>
            {/each}
          </div>
        </div>

        {#if filteredMemories.length === 0}
          <p class="text-xs text-text-secondary py-8 text-center">
            {memories.length === 0 ? t("mem.noneInProject") : t("mem.noMatch")}
          </p>
        {:else}
          <div class="space-y-1.5">
            {#each filteredMemories as m}
              {@const type = m.filename === "MEMORY.md" ? "index" : ((m.frontmatter?.type ?? "unknown"))}
              <div class="bg-bg-secondary border border-border rounded-xl p-3 hover:border-border-hover transition-colors group">
                <div class="flex items-start gap-3">
                  <button onclick={() => viewMemory(m)} class="flex-1 text-left min-w-0">
                    <div class="flex items-center gap-2">
                      <span class="text-[10px] px-1.5 py-0.5 rounded-full font-medium {typeColors[type] ?? 'bg-bg-tertiary text-text-muted'}">
                        {typeLabel(type)}
                      </span>
                      <span class="text-xs font-medium truncate">{m.frontmatter?.name ?? m.filename}</span>
                    </div>
                    {#if m.frontmatter?.description}
                      <p class="text-[11px] text-text-secondary mt-1 line-clamp-2">{m.frontmatter.description}</p>
                    {/if}
                    <div class="text-[10px] text-text-muted mt-1" title={m.filename}>{memoryIdentity(m, type)}</div>
                  </button>
                  <div class="shrink-0 flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
                    {#if canWriteMemory(m.source)}
                      <button onclick={() => startEditing(m)}
                        class="text-[10px] px-2 py-1 border border-border rounded-lg hover:bg-bg-hover">{t("common.edit")}</button>
                      <button onclick={() => (confirmDelete = m.filename)}
                        class="text-[10px] px-2 py-1 border border-danger/30 text-danger rounded-lg hover:bg-danger-dim">{t("mem.delete")}</button>
                    {:else}
                      <button onclick={() => viewMemory(m)}
                        class="text-[10px] px-2 py-1 border border-border rounded-lg hover:bg-bg-hover">{t("mem.view")}</button>
                    {/if}
                  </div>
                </div>
              </div>
            {/each}
          </div>
        {/if}
      </div>
    {:else}
      <div class="flex items-center justify-center h-full text-sm text-text-secondary">
        {t("mem.selectProject")}
      </div>
    {/if}
  </div>

  <ConfirmDialog
    open={deleteTarget !== null}
    tone="danger"
    title={t("mem.deleteTitle")}
    message={deleteTarget
      ? t("mem.deleteMsg", { name: deleteTarget.frontmatter?.name ?? deleteTarget.filename })
      : ""}
    detail={deleteTarget ? `${deleteTarget.project_path} · ${deleteTarget.filename}` : null}
    confirmLabel={t("mem.delete")}
    busy={deleting}
    onconfirm={() => deleteTarget && doDelete(deleteTarget)}
    oncancel={() => (confirmDelete = null)}
  />
</div>
