<script lang="ts">
  import { onMount } from "svelte";
  import { check, type DownloadEvent, type Update } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
  import { api } from "$lib/api";
  import { t } from "$lib/i18n.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import Markdown from "$lib/Markdown.svelte";
  import { DEFAULT_PRICES } from "$lib/prices";
  import type { DejavuConfig, InstructionArtifact, InstructionDetail, SourceInfo } from "$lib/types";
  import packageInfo from "../../../package.json";
  import changelog from "../../../CHANGELOG.md?raw";

  let dejavuConfig = $state<DejavuConfig | null>(null);
  let sources = $state<SourceInfo[]>([]);
  let configArtifacts = $state<InstructionArtifact[]>([]);
  let selectedConfig = $state<InstructionArtifact | null>(null);
  let configDetail = $state<InstructionDetail | null>(null);
  let configContent = $state("");
  let loading = $state(true);
  let loadingConfig = $state(false);
  let saving = $state(false);
  let savingConfig = $state(false);
  let error = $state("");
  let saved = $state("");
  let configSaved = $state(false);
  let configEditing = $state(false);
  let updateStatus = $state<"idle" | "checking" | "latest" | "available" | "downloading" | "ready" | "error">("idle");
  let availableUpdate = $state.raw<Update | null>(null);
  let updateError = $state("");
  let downloadedBytes = $state(0);
  let downloadTotal = $state<number | null>(null);

  let newEnvKey = $state("");
  let newEnvVal = $state("");
  let newArgs = $state<Record<string, string>>({});
  let newExcludedDistro = $state("");

  // Every non-native host any source reports, deduped — the WSL installs actually in use.
  let connectedHosts = $derived([...new Set(sources.flatMap((source) => source.hosts ?? []))].sort());

  function addExcludedDistro() {
    const name = newExcludedDistro.trim();
    if (!dejavuConfig || !name) return;
    if (!dejavuConfig.wsl_excluded.some((existing) => existing.toLowerCase() === name.toLowerCase())) {
      dejavuConfig.wsl_excluded = [...dejavuConfig.wsl_excluded, name];
    }
    newExcludedDistro = "";
  }

  function removeExcludedDistro(name: string) {
    if (!dejavuConfig) return;
    dejavuConfig.wsl_excluded = dejavuConfig.wsl_excluded.filter((existing) => existing !== name);
  }

  // Usage price table — stored in the app config (DejavuConfig.prices), edited here.
  let pricesSaved = $state(false);

  function addPriceRow() {
    if (!dejavuConfig) return;
    dejavuConfig.prices = [...(dejavuConfig.prices ?? []), { match: "", input: 0, output: 0 }];
  }
  function removePriceRow(index: number) {
    if (!dejavuConfig) return;
    dejavuConfig.prices = (dejavuConfig.prices ?? []).filter((_, i) => i !== index);
  }
  async function savePrices() {
    if (!dejavuConfig) return;
    dejavuConfig.prices = (dejavuConfig.prices ?? [])
      .filter((r) => r.match.trim())
      .map((r) => ({ match: r.match.trim(), input: Number(r.input) || 0, output: Number(r.output) || 0 }));
    await saveDejavuConfig();
    pricesSaved = true;
    setTimeout(() => (pricesSaved = false), 2000);
  }
  function resetPricesToDefault() {
    if (!dejavuConfig) return;
    dejavuConfig.prices = DEFAULT_PRICES.map((r) => ({ ...r }));
  }

  const argPresets: Record<string, string[]> = {
    claude: ["--dangerously-skip-permissions", "--verbose", "--effort max"],
    codex: [
      "--search",
      "--no-alt-screen",
      "--ask-for-approval on-request",
      "--sandbox workspace-write",
      "--dangerously-bypass-approvals-and-sandbox",
    ],
    opencode: ["--fork", "--print-logs", "--log-level DEBUG", "--pure"],
  };

  let resumableSources = $derived(sources.filter((source) => source.capabilities.sessions_resume));
  let configSources = $derived(
    sources.filter((source) => configArtifacts.some((artifact) => artifact.source === source.id))
  );
  let configLineCount = $derived(configContent ? configContent.split("\n").length : 0);

  async function refresh() {
    loading = true;
    error = "";
    try {
      const [dc, sourceList, artifactList] = await Promise.all([
        api.dejavu.getConfig(),
        api.sessions.listSources(),
        api.instructions.list(),
      ]);
      const configs = artifactList.filter((artifact) => artifact.kind === "config" && artifact.scope !== "project");
      dejavuConfig = {
        ...dc,
        agent_args: dc.agent_args ?? {},
        prices: dc.prices ?? [],
        wsl_scan: dc.wsl_scan ?? true,
        wsl_excluded: dc.wsl_excluded ?? [],
      };
      sources = sourceList;
      configArtifacts = configs;

      const current = selectedConfig
        ? configs.find(
            (artifact) => artifact.source === selectedConfig?.source && artifact.path === selectedConfig?.path
          )
        : null;
      const next = current ?? configs[0] ?? null;
      if (next) await openConfig(next);
      else {
        selectedConfig = null;
        configDetail = null;
        configContent = "";
      }
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function saveDejavuConfig() {
    if (!dejavuConfig) return;
    saving = true;
    saved = "";
    error = "";
    try {
      await api.dejavu.saveConfig(dejavuConfig);
      saved = "dejavu";
      setTimeout(() => (saved = ""), 2000);
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  async function openConfig(artifact: InstructionArtifact) {
    selectedConfig = artifact;
    configEditing = false;
    configSaved = false;
    loadingConfig = true;
    error = "";
    try {
      configDetail = await api.instructions.get(artifact.source, artifact.path);
      selectedConfig = configDetail;
      configContent = configDetail.content;
    } catch (e) {
      error = String(e);
    } finally {
      loadingConfig = false;
    }
  }

  async function saveConfigArtifact() {
    if (!selectedConfig?.editable) return;
    savingConfig = true;
    configSaved = false;
    error = "";
    try {
      await api.instructions.save(selectedConfig.source, selectedConfig.path, configContent);
      configEditing = false;
      await refresh();
      configSaved = true;
      setTimeout(() => (configSaved = false), 2000);
    } catch (e) {
      error = String(e);
    } finally {
      savingConfig = false;
    }
  }

  function addEnv() {
    if (!dejavuConfig || !newEnvKey.trim()) return;
    dejavuConfig.env = { ...dejavuConfig.env, [newEnvKey.trim()]: newEnvVal };
    newEnvKey = "";
    newEnvVal = "";
  }

  function removeEnv(key: string) {
    if (!dejavuConfig) return;
    const copy = { ...dejavuConfig.env };
    delete copy[key];
    dejavuConfig.env = copy;
  }

  function argsFor(sourceId: string): string[] {
    return dejavuConfig?.agent_args?.[sourceId] ?? [];
  }

  function setArgs(sourceId: string, args: string[]) {
    if (!dejavuConfig) return;
    dejavuConfig.agent_args = { ...(dejavuConfig.agent_args ?? {}), [sourceId]: args };
  }

  function addAgentArg(sourceId: string) {
    const value = (newArgs[sourceId] ?? "").trim();
    if (!value) return;
    setArgs(sourceId, [...argsFor(sourceId), value]);
    newArgs = { ...newArgs, [sourceId]: "" };
  }

  function addPresetArg(sourceId: string, value: string) {
    const args = argsFor(sourceId);
    if (args.includes(value)) return;
    setArgs(sourceId, [...args, value]);
  }

  function removeAgentArg(sourceId: string, index: number) {
    setArgs(sourceId, argsFor(sourceId).filter((_, i) => i !== index));
  }

  function presetsFor(sourceId: string) {
    return argPresets[sourceId] ?? [];
  }

  function configsFor(sourceId: string): InstructionArtifact[] {
    return configArtifacts.filter((artifact) => artifact.source === sourceId);
  }

  function isSelectedConfig(artifact: InstructionArtifact): boolean {
    return selectedConfig?.source === artifact.source && selectedConfig?.path === artifact.path;
  }

  function sourceName(sourceId?: string | null): string {
    return sources.find((source) => source.id === sourceId)?.display_name ?? t("set.fallbackConfig");
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

  function formatSize(bytes: number): string {
    if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
  }

  function updateStatusText(): string {
    switch (updateStatus) {
      case "checking": return t("set.updateChecking");
      case "latest": return t("set.updateLatest");
      case "available": return t("set.updateAvailable", { version: availableUpdate?.version ?? "" });
      case "downloading": return downloadTotal
        ? t("set.updateDownloadingPercent", { percent: Math.min(100, Math.round(downloadedBytes / downloadTotal * 100)) })
        : t("set.updateDownloading", { size: formatSize(downloadedBytes) });
      case "ready": return t("set.updateReady");
      case "error": return updateError || t("set.updateFailed");
      default: return t("set.updateIdle");
    }
  }

  async function checkForUpdate() {
    updateStatus = "checking";
    updateError = "";
    try {
      if (availableUpdate) await availableUpdate.close();
      availableUpdate = await check({ timeout: 15_000 });
      updateStatus = availableUpdate ? "available" : "latest";
    } catch (e) {
      availableUpdate = null;
      updateStatus = "error";
      updateError = String(e);
    }
  }

  async function installUpdate() {
    if (!availableUpdate) return;
    updateStatus = "downloading";
    updateError = "";
    downloadedBytes = 0;
    downloadTotal = null;
    try {
      await availableUpdate.downloadAndInstall((event: DownloadEvent) => {
        if (event.event === "Started") {
          downloadTotal = event.data.contentLength ?? null;
        } else if (event.event === "Progress") {
          downloadedBytes += event.data.chunkLength;
        }
      });
      updateStatus = "ready";
    } catch (e) {
      updateStatus = "error";
      updateError = String(e);
    }
  }

  async function restartApp() {
    await relaunch();
  }

  onMount(() => {
    deferRouteLoad(refresh);
  });
</script>

<div class="h-full overflow-y-auto p-6">
  {#if error}
    <div class="mb-4 rounded-xl border border-danger/30 bg-danger-dim p-3 text-sm text-danger">{error}</div>
  {/if}

  <section class="mb-8">
    <div class="mb-4">
      <h2 class="text-lg font-semibold">{t("set.aboutUpdate")}</h2>
      <p class="mt-1 text-xs text-text-muted">{t("set.aboutUpdateSub")}</p>
    </div>
    <div class="rounded-xl border border-border bg-bg-secondary p-4">
      <div class="flex items-start justify-between gap-4">
        <div class="min-w-0">
          <div class="flex flex-wrap items-center gap-2">
            <span class="text-sm font-medium">Code Déjà Vu</span>
            <span class="rounded-md bg-bg-tertiary px-2 py-0.5 font-mono text-[10px] text-text-secondary">v{packageInfo.version}</span>
            {#if availableUpdate}
              <span class="rounded-md bg-accent-dim px-2 py-0.5 font-mono text-[10px] text-accent">v{availableUpdate.version}</span>
            {/if}
          </div>
          <p class="mt-2 break-words text-xs {updateStatus === 'error' ? 'text-danger' : 'text-text-secondary'}">{updateStatusText()}</p>
          {#if availableUpdate?.body}
            <div class="mt-3 whitespace-pre-wrap rounded-lg bg-bg-tertiary p-3 text-xs leading-relaxed text-text-secondary">{availableUpdate.body}</div>
          {/if}
        </div>
        <div class="flex shrink-0 items-center gap-2">
          {#if updateStatus === "available"}
            <button onclick={installUpdate} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover">
              {t("set.updateInstall")}
            </button>
          {:else if updateStatus === "ready"}
            <button onclick={restartApp} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover">
              {t("set.updateRestart")}
            </button>
          {:else}
            <button
              onclick={checkForUpdate}
              disabled={updateStatus === "checking" || updateStatus === "downloading"}
              class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover disabled:opacity-50"
            >
              {updateStatus === "checking" ? t("set.updateCheckingShort") : t("set.updateCheck")}
            </button>
          {/if}
        </div>
      </div>

      {#if updateStatus === "downloading"}
        <div class="mt-3 h-1.5 overflow-hidden rounded-full bg-bg-tertiary">
          <div
            class="h-full rounded-full bg-accent transition-[width]"
            style={`width: ${downloadTotal ? Math.min(100, downloadedBytes / downloadTotal * 100) : 12}%`}
          ></div>
        </div>
      {/if}

      <details class="mt-4 border-t border-border-subtle pt-3">
        <summary class="cursor-pointer text-xs font-medium text-text-secondary hover:text-text">{t("set.changelog")}</summary>
        <div class="mt-3 max-h-80 overflow-auto rounded-lg bg-bg p-3 text-text-secondary">
          <Markdown content={changelog} />
        </div>
      </details>
    </div>
  </section>

  {#if loading}
    <p class="text-sm text-text-secondary">{t("common.loading")}</p>
  {:else}
    <div class="mb-8">
      <div class="mb-4 flex items-center justify-between">
        <h2 class="text-lg font-semibold">{t("set.title")}</h2>
        <button
          onclick={saveDejavuConfig}
          disabled={saving}
          class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50"
        >
          {saving ? "..." : saved === "dejavu" ? t("common.saved") : t("common.save")}
        </button>
      </div>

      {#if dejavuConfig}
        <div class="space-y-4">
          <div class="rounded-xl border border-border bg-bg-secondary p-4">
            <h3 class="mb-3 text-sm font-medium">{t("set.terminal")}</h3>
            <div>
              <label for="shell" class="mb-1 block text-xs text-text-secondary">{t("set.shellType")}</label>
              <select
                id="shell"
                bind:value={dejavuConfig.shell}
                class="w-full rounded-lg border border-border bg-bg px-2.5 py-1.5 text-xs outline-none focus:border-accent"
              >
                <option value="pwsh">PowerShell (pwsh)</option>
                <option value="powershell">Windows PowerShell</option>
                <option value="cmd">CMD</option>
                <option value="bash">Bash / Git Bash</option>
              </select>
            </div>
          </div>

          <div class="rounded-xl border border-border bg-bg-secondary p-4">
            <h3 class="mb-3 text-sm font-medium">{t("set.wsl")}</h3>
            <label class="flex items-start gap-2.5">
              <input type="checkbox" bind:checked={dejavuConfig.wsl_scan} class="mt-0.5 accent-accent" />
              <span>
                <span class="block text-xs">{t("set.wslScan")}</span>
                <span class="mt-1 block text-[10px] leading-relaxed text-text-muted">{t("set.wslScanHint")}</span>
              </span>
            </label>

            {#if dejavuConfig.wsl_scan}
              <div class="mt-3 border-t border-border-subtle pt-3">
                <div class="mb-2 text-[10px] text-text-secondary">{t("set.wslConnected")}</div>
                {#if connectedHosts.length > 0}
                  <div class="flex flex-wrap gap-1.5">
                    {#each connectedHosts as host}
                      <span class="rounded-lg bg-accent-dim px-2.5 py-1 font-mono text-[11px] text-accent">{host}</span>
                    {/each}
                  </div>
                {:else}
                  <p class="text-[11px] text-text-muted">{t("set.wslNone")}</p>
                {/if}
              </div>

              <div class="mt-3 border-t border-border-subtle pt-3">
                <div class="mb-2 text-[10px] text-text-secondary">{t("set.wslExcluded")}</div>
                {#if dejavuConfig.wsl_excluded.length > 0}
                  <div class="mb-2 flex flex-wrap gap-1.5">
                    {#each dejavuConfig.wsl_excluded as distro}
                      <span class="flex items-center gap-1.5 rounded-lg bg-bg-tertiary px-2.5 py-1 font-mono text-[11px]">
                        {distro}
                        <button onclick={() => removeExcludedDistro(distro)} class="text-[10px] text-danger hover:text-danger-hover">x</button>
                      </span>
                    {/each}
                  </div>
                {/if}
                <div class="flex gap-1">
                  <input
                    bind:value={newExcludedDistro}
                    placeholder={t("set.wslExcludePlaceholder")}
                    class="w-48 rounded-lg border border-border bg-bg px-2 py-1 font-mono text-[10px] outline-none focus:border-accent"
                  />
                  <button onclick={addExcludedDistro} class="rounded-lg border border-border px-2 py-1 text-[10px] hover:bg-bg-hover">{t("common.add")}</button>
                </div>
              </div>
            {/if}
          </div>

          <div class="rounded-xl border border-border bg-bg-secondary p-4">
            <h3 class="mb-3 text-sm font-medium">{t("set.resumeArgs")}</h3>
            <div class="space-y-3">
              {#each resumableSources as source}
                {@const args = argsFor(source.id)}
                <div class="border-t border-border-subtle pt-3 first:border-t-0 first:pt-0">
                  <div class="mb-2 flex items-center justify-between gap-3">
                    <div>
                      <div class="text-xs font-medium">{source.display_name}</div>
                      <div class="mt-0.5 text-[10px] text-text-muted">
                        {source.available ? t("set.resumable") : t("set.noLocalData")}
                      </div>
                    </div>
                    <span class="font-mono text-[10px] text-text-muted">{t("set.itemsN", { n: args.length })}</span>
                  </div>

                  {#if args.length > 0}
                    <div class="mb-2 flex flex-wrap gap-1.5">
                      {#each args as arg, i}
                        <span class="flex items-center gap-1.5 rounded-lg bg-bg-tertiary px-2.5 py-1 font-mono text-[11px]">
                          {arg}
                          <button onclick={() => removeAgentArg(source.id, i)} class="text-[10px] text-danger hover:text-danger-hover">x</button>
                        </span>
                      {/each}
                    </div>
                  {/if}

                  {#if presetsFor(source.id).some((preset) => !args.includes(preset))}
                    <div class="mb-2 flex flex-wrap gap-1.5">
                      {#each presetsFor(source.id) as preset}
                        {#if !args.includes(preset)}
                          <button
                            onclick={() => addPresetArg(source.id, preset)}
                            class="rounded-lg border border-border px-2 py-1 text-[10px] hover:bg-bg-hover"
                          >
                            + {t("set.arg." + preset)}
                          </button>
                        {/if}
                      {/each}
                    </div>
                  {/if}

                  <div class="flex gap-1">
                    <input
                      value={newArgs[source.id] ?? ""}
                      oninput={(e) => (newArgs = { ...newArgs, [source.id]: (e.target as HTMLInputElement).value })}
                      placeholder={t("set.customArg")}
                      class="w-48 rounded-lg border border-border bg-bg px-2 py-1 font-mono text-[10px] outline-none focus:border-accent"
                    />
                    <button onclick={() => addAgentArg(source.id)} class="rounded-lg border border-border px-2 py-1 text-[10px] hover:bg-bg-hover">{t("common.add")}</button>
                  </div>
                </div>
              {/each}
            </div>
          </div>

          <div class="rounded-xl border border-border bg-bg-secondary p-4">
            <h3 class="mb-3 text-sm font-medium">{t("set.envVars")}</h3>
            {#if Object.keys(dejavuConfig.env).length > 0}
              <div class="mb-3 space-y-1.5">
                {#each Object.entries(dejavuConfig.env) as [key, val]}
                  <div class="flex items-center gap-2 rounded-lg bg-bg-tertiary px-3 py-1.5">
                    <span class="min-w-[120px] text-xs font-mono font-medium text-accent">{key}</span>
                    <span class="flex-1 truncate font-mono text-xs text-text-secondary">{val}</span>
                    <button onclick={() => removeEnv(key)} class="shrink-0 rounded px-1.5 py-0.5 text-[10px] text-danger hover:bg-danger-dim">{t("common.delete")}</button>
                  </div>
                {/each}
              </div>
            {/if}

            <div class="flex gap-2">
              <input
                bind:value={newEnvKey}
                placeholder={t("set.envNamePlaceholder")}
                class="flex-1 rounded-lg border border-border bg-bg px-2.5 py-1.5 font-mono text-xs outline-none focus:border-accent"
              />
              <input
                bind:value={newEnvVal}
                placeholder={t("set.envValPlaceholder")}
                class="flex-1 rounded-lg border border-border bg-bg px-2.5 py-1.5 font-mono text-xs outline-none focus:border-accent"
              />
              <button onclick={addEnv} class="shrink-0 rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("common.add")}</button>
            </div>
          </div>
        </div>
      {/if}
    </div>

    <section class="mb-8">
      <div class="mb-4 flex items-center justify-between gap-4">
        <div>
          <h2 class="text-lg font-semibold">{t("set.prices")}</h2>
          <p class="mt-1 text-xs text-text-muted">{t("set.pricesSub")}</p>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <button onclick={resetPricesToDefault} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("set.priceReset")}</button>
          <button onclick={savePrices} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover">{pricesSaved ? t("common.saved") : t("common.save")}</button>
        </div>
      </div>

      <div class="rounded-xl border border-border bg-bg-secondary p-4">
        <div class="mb-2 grid grid-cols-[1fr_6.5rem_6.5rem_2rem] gap-2 text-[10px] uppercase tracking-wider text-text-muted">
          <span>{t("set.priceMatch")}</span>
          <span class="text-right">{t("set.priceInput")}</span>
          <span class="text-right">{t("set.priceOutput")}</span>
          <span></span>
        </div>
        {#if (dejavuConfig?.prices?.length ?? 0) === 0}
          <p class="py-2 text-xs text-text-secondary">{t("set.pricesEmpty")}</p>
        {/if}
        <div class="space-y-1.5">
          {#each dejavuConfig?.prices ?? [] as row, i (i)}
            <div class="grid grid-cols-[1fr_6.5rem_6.5rem_2rem] items-center gap-2">
              <input
                bind:value={row.match}
                placeholder={t("set.priceMatchPlaceholder")}
                class="rounded-lg border border-border bg-bg px-2.5 py-1.5 font-mono text-xs outline-none focus:border-accent"
              />
              <input
                type="number"
                step="0.01"
                min="0"
                bind:value={row.input}
                class="rounded-lg border border-border bg-bg px-2.5 py-1.5 text-right font-mono text-xs outline-none focus:border-accent"
              />
              <input
                type="number"
                step="0.01"
                min="0"
                bind:value={row.output}
                class="rounded-lg border border-border bg-bg px-2.5 py-1.5 text-right font-mono text-xs outline-none focus:border-accent"
              />
              <button onclick={() => removePriceRow(i)} class="rounded px-1.5 py-0.5 text-[10px] text-danger hover:bg-danger-dim" title={t("common.delete")} aria-label={t("common.delete")}>x</button>
            </div>
          {/each}
        </div>
        <button onclick={addPriceRow} class="mt-3 rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("set.priceAdd")}</button>
      </div>
    </section>

    <div>
      <div class="mb-4 flex items-center justify-between gap-4">
        <div>
          <h2 class="text-lg font-semibold">{t("set.toolsGlobal")}</h2>
          <p class="mt-1 text-xs text-text-muted">{t("set.toolsGlobalSub")}</p>
        </div>
        {#if selectedConfig}
          <span class="shrink-0 rounded-md px-2 py-1 text-[10px] {selectedConfig.editable ? 'bg-success-dim text-success' : 'bg-bg-tertiary text-text-muted'}">
            {selectedConfig.editable ? t("common.editable") : t("common.readonly")}
          </span>
        {/if}
      </div>

      <div class="grid min-h-[520px] grid-cols-[300px_minmax(0,1fr)] overflow-hidden rounded-xl border border-border bg-bg-secondary">
        <aside class="overflow-y-auto border-r border-border bg-bg-secondary">
          {#if configSources.length === 0}
            <p class="p-4 text-sm text-text-secondary">{t("set.noGlobalConfig")}</p>
          {:else}
            <div class="divide-y divide-border-subtle">
              {#each configSources as source}
                {@const artifacts = configsFor(source.id)}
                <section class="px-3 py-3">
                  <div class="mb-2 flex items-center justify-between gap-2">
                    <div class="min-w-0">
                      <div class="truncate text-xs font-medium">{source.display_name}</div>
                      <div class="mt-0.5 text-[10px] text-text-muted">{t("set.globalFilesN", { n: artifacts.length })}</div>
                    </div>
                    <span class="shrink-0 rounded-md px-1.5 py-0.5 text-[10px] font-medium {sourceStateClass(source)}">
                      {sourceState(source)}
                    </span>
                  </div>

                  <div class="space-y-1">
                    {#each artifacts as artifact}
                      <button
                        onclick={() => openConfig(artifact)}
                        class="w-full rounded-lg px-2.5 py-2 text-left transition-colors
                          {isSelectedConfig(artifact) ? 'bg-accent-dim text-accent' : 'text-text-secondary hover:bg-bg-hover'}"
                      >
                        <div class="flex items-center justify-between gap-2">
                          <span class="truncate text-xs font-medium">{artifact.title}</span>
                          <span class="shrink-0 rounded bg-bg px-1.5 py-0.5 text-[9px] text-text-muted">{t("common.global")}</span>
                        </div>
                        <div class="mt-0.5 truncate font-mono text-[10px] text-text-muted">{artifact.path}</div>
                      </button>
                    {/each}
                  </div>
                </section>
              {/each}
            </div>
          {/if}
        </aside>

        <section class="flex min-w-0 flex-col bg-bg">
          <div class="flex items-center justify-between gap-4 border-b border-border px-4 py-3">
            <div class="min-w-0">
              <div class="flex items-center gap-2">
                <h3 class="truncate text-sm font-semibold">{selectedConfig?.title ?? t("set.fallbackConfig")}</h3>
                {#if selectedConfig && !selectedConfig.exists}
                  <span class="rounded-md bg-warning-dim px-1.5 py-0.5 text-[10px] text-warning">{t("common.notCreated")}</span>
                {/if}
              </div>
              <div class="mt-0.5 truncate font-mono text-[10px] text-text-muted">
                {selectedConfig ? `${sourceName(selectedConfig.source)} · ${selectedConfig.path}` : t("set.selectConfigHint")}
              </div>
            </div>

            <div class="flex shrink-0 items-center gap-2">
              {#if selectedConfig}
                <span class="text-xs text-text-secondary">{t("set.linesN", { n: configLineCount })} · {formatSize(selectedConfig.size_bytes)}</span>
              {/if}
              {#if selectedConfig?.editable}
                {#if configEditing}
                  <button
                    onclick={saveConfigArtifact}
                    disabled={savingConfig}
                    class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50"
                  >
                    {savingConfig ? t("common.saving") : t("common.save")}
                  </button>
                  <button onclick={() => selectedConfig && openConfig(selectedConfig)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">
                    {t("common.cancel")}
                  </button>
                {:else}
                  <button onclick={() => (configEditing = true)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">
                    {t("common.edit")}
                  </button>
                {/if}
              {/if}
            </div>
          </div>

          {#if configSaved}
            <div class="mx-4 mt-3 rounded-lg border border-success/30 bg-success-dim p-3 text-sm text-success">{t("common.saved")}</div>
          {/if}

          {#if loadingConfig}
            <p class="p-4 text-sm text-text-secondary">{t("common.loading")}</p>
          {:else if !selectedConfig}
            <div class="flex flex-1 items-center justify-center text-sm text-text-secondary">{t("set.noGlobalConfig")}</div>
          {:else}
            <textarea
              bind:value={configContent}
              readonly={!configEditing}
              spellcheck="false"
              class="flex-1 resize-none bg-bg px-4 py-4 font-mono text-xs leading-relaxed outline-none {configEditing ? 'text-text' : 'text-text-secondary'}"
            ></textarea>
          {/if}
        </section>
      </div>
    </div>
  {/if}
</div>
