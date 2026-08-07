<script lang="ts">
  import type { SessionRecord, SessionSummary, SubagentInfo } from "$lib/types";
  import { api } from "$lib/api";
  import { t } from "$lib/i18n.svelte";
  import { pushToast } from "$lib/toast.svelte";
  import Markdown from "$lib/Markdown.svelte";
  import ToolCall from "$lib/ToolCall.svelte";
  import ThinkingBlock from "$lib/ThinkingBlock.svelte";
  import MetaBlock from "$lib/MetaBlock.svelte";
  import { highlightPlain } from "$lib/html";
  import Timeline from "$lib/Timeline.svelte"; // recursive: a subagent is just another timeline

  let {
    records,
    highlight = "",
    session = null,
    subagents = [],
  }: {
    records: SessionRecord[];
    highlight?: string;
    session?: SessionSummary | null;
    subagents?: SubagentInfo[];
  } = $props();

  let expandedCards = $state<Record<string, boolean>>({});
  function toggleCard(k: string) {
    expandedCards = { ...expandedCards, [k]: !expandedCards[k] };
  }
  // `byte_offset` points to a source line, not a unique rendered card. Codex/OpenCode
  // sessions can produce many tool records from one event/line, so key expand state
  // by tool id when available and by object identity as a fallback.
  const objectKeys = new WeakMap<SessionRecord, string>();
  let nextObjectKey = 0;
  function cardKey(r: SessionRecord, role = "tool"): string {
    if (r.tool_use_id) return [role, r.record_type, r.tool_name ?? "", r.tool_use_id].join("::");
    let k = objectKeys.get(r);
    if (!k) {
      k = `obj::${++nextObjectKey}`;
      objectKeys.set(r, k);
    }
    return `${role}::${k}`;
  }

  // Per-message copy with a brief ✓ confirmation, keyed by the record's card key.
  let copiedId = $state("");
  async function copyText(text: string, id: string) {
    try {
      await navigator.clipboard.writeText(text);
      copiedId = id;
      setTimeout(() => {
        if (copiedId === id) copiedId = "";
      }, 1200);
    } catch {
      pushToast(t("toast.copyFailed"));
    }
  }

  interface ExpandedAgentPage {
    records: SessionRecord[];
    nextOffset: number;
    hasMore: boolean;
  }
  let expandedAgents = $state<Record<string, ExpandedAgentPage>>({});
  let loadingAgent = $state<string | null>(null);
  async function toggleAgent(agent: SubagentInfo) {
    if (expandedAgents[agent.agent_id]) {
      const c = { ...expandedAgents };
      delete c[agent.agent_id];
      expandedAgents = c;
      return;
    }
    if (!session) return;
    await loadAgentPage(agent, true);
  }

  async function loadAgentPage(agent: SubagentInfo, reset = false) {
    if (!session || loadingAgent === agent.agent_id) return;
    loadingAgent = agent.agent_id;
    try {
      const current = reset ? undefined : expandedAgents[agent.agent_id];
      const offset = current?.nextOffset ?? 0;
      const res = await api.sessions.getSubagentDetail(
        session.project, session.session_id, agent.agent_id, offset, 200,
        session.archive_name, session.source
      );
      expandedAgents = {
        ...expandedAgents,
        [agent.agent_id]: {
          records: [...(current?.records ?? []), ...res.records],
          nextOffset: res.next_byte_offset,
          hasMore: res.has_more && res.next_byte_offset > offset,
        },
      };
    } catch (e) {
      console.error(e);
    } finally {
      loadingAgent = null;
    }
  }

  const fmt = (ts: string | null | undefined) => ts ?? "";

  let callById = $derived.by(() => {
    const m = new Map<string, SessionRecord>();
    for (const r of records) if (r.tool_name && r.tool_use_id) m.set(r.tool_use_id, r);
    return m;
  });
  // A tool's result (and AskUserQuestion's answer) merges back into its call card.
  let resultById = $derived.by(() => {
    const m = new Map<string, SessionRecord>();
    for (const r of records) if (r.record_type === "tool_result" && r.tool_use_id) m.set(r.tool_use_id, r);
    return m;
  });
  // Pre/PostToolUse hooks per call: Pre renders above the result, Post below it.
  let preHooksByCall = $derived.by(() => groupHooks("🪝 Pre"));
  let postHooksByCall = $derived.by(() => groupHooks("🪝 Post"));
  function groupHooks(prefix: string) {
    const m = new Map<string, SessionRecord[]>();
    const seen = new Map<string, Set<string>>(); // dedupe identical hooks (same label) per tool
    for (const r of records) if (r.record_type === "hook" && r.tool_use_id && r.content_preview.startsWith(prefix)) {
      const label = r.content_preview.split("\n")[0];
      let s = seen.get(r.tool_use_id);
      if (!s) { s = new Set(); seen.set(r.tool_use_id, s); }
      if (s.has(label)) continue;
      s.add(label);
      const a = m.get(r.tool_use_id) ?? [];
      a.push(r);
      m.set(r.tool_use_id, a);
    }
    return m;
  }
  const isAgentCall = (r: SessionRecord) => r.tool_name === "Agent" || r.tool_name === "spawn_agent";
  // Parallel batches: consecutive tool calls from one model turn (group_id), laid out
  // horizontally. Each call card is self-contained (input → Pre → result → Post).
  // Keyed by the record OBJECT (not byte_offset) — a lead-in text record and the tool record
  // from the same source line share a byte_offset, which would otherwise render the batch twice.
  let parallel = $derived.by(() => {
    const starts = new Map<SessionRecord, SessionRecord[]>();
    const skip = new Set<SessionRecord>();
    const isCall = (r: SessionRecord) =>
      !!r.tool_name && r.record_type === "assistant" && !isAgentCall(r) && r.tool_name !== "AskUserQuestion" && !!r.group_id;
    let i = 0;
    while (i < records.length) {
      const r = records[i];
      if (isCall(r)) {
        const batch = [r];
        let j = i + 1;
        while (j < records.length && isCall(records[j]) && records[j].group_id === r.group_id) { batch.push(records[j]); j++; }
        if (batch.length > 1) { starts.set(r, batch); for (let k = 1; k < batch.length; k++) skip.add(batch[k]); i = j; continue; }
      }
      i++;
    }
    return { starts, skip };
  });

  const agentOf = (r: SessionRecord): SubagentInfo | null =>
    isAgentCall(r) && r.tool_use_id ? subagents.find((a) => a.tool_use_id === r.tool_use_id) ?? null : null;
  const resultFor = (r: SessionRecord) => (r.tool_use_id ? resultById.get(r.tool_use_id) ?? null : null);
  const preFor = (r: SessionRecord) => (r.tool_use_id ? preHooksByCall.get(r.tool_use_id) ?? null : null);
  const postFor = (r: SessionRecord) => (r.tool_use_id ? postHooksByCall.get(r.tool_use_id) ?? null : null);
</script>

<div class="space-y-2">
  {#each records as r}
    {@const matchedAgent = agentOf(r)}

    {#if r.record_type === "hook"}
      <!-- hung on its call card, not shown standalone -->

    {:else if parallel.skip.has(r)}
      <!-- later member of a parallel batch (rendered by the first) -->

    {:else if parallel.starts.has(r)}
      {@const batch = parallel.starts.get(r) ?? []}
      <div class="mx-4">
        <div class="text-[9px] text-text-muted mb-1 pl-1">⫶ {t("tool.parallelCalls", { n: batch.length })}</div>
        <!-- Side-by-side when each card gets ≥22rem; collapses to ONE full-width column when the
             screen is too narrow. Titles ellipsis-clip to the column (one line); full content on expand. -->
        <div class="grid gap-2 items-start" style="grid-template-columns: repeat(auto-fill, minmax(min(100%, 22rem), 1fr));">
          {#each batch as br}
            {@const k = cardKey(br)}
            <div class="min-w-0">
              <ToolCall record={br} result={resultFor(br)} preHooks={preFor(br)} postHooks={postFor(br)}
                expanded={!!expandedCards[k]} onToggle={() => toggleCard(k)} {highlight} />
            </div>
          {/each}
        </div>
      </div>

    {:else if r.record_type === "meta"}
      <MetaBlock content={r.content_preview} timestamp={fmt(r.timestamp)} />

    {:else if r.level === "debug"}
      <!-- Raw debug record — full content, collapsed by default (never truncated) -->
      <MetaBlock content={r.content_preview ? r.record_type + "  " + r.content_preview : r.record_type} timestamp={fmt(r.timestamp)} />

    {:else if r.record_type === "thinking"}
      <ThinkingBlock content={r.content_preview} {highlight} />

    {:else if r.record_type === "user"}
      {@const mid = cardKey(r, "msg")}
      <div class="flex justify-end group">
        <div class="max-w-[75%]">
          <div class="flex items-center justify-end gap-1.5 mb-0.5 pr-1">
            <button onclick={() => copyText(r.content_preview, mid)} title={t("tool.copyMessage")}
              class="opacity-0 group-hover:opacity-100 transition-opacity text-[11px] leading-none {copiedId === mid ? 'text-success' : 'text-text-muted hover:text-text'}">{copiedId === mid ? "✓" : "⧉"}</button>
            <span class="text-[10px] text-text-muted">{fmt(r.timestamp)}</span>
          </div>
          <div class="bg-accent text-white rounded-2xl rounded-tr-sm px-4 py-2.5">
            <pre class="whitespace-pre-wrap break-words text-[12px] leading-relaxed font-sans">{@html highlightPlain(r.content_preview, highlight)}</pre>
          </div>
        </div>
      </div>

    {:else if r.record_type === "assistant" && !r.tool_name}
      {@const mid = cardKey(r, "msg")}
      <div class="flex justify-start group">
        <div class="max-w-[75%]">
          <div class="flex items-center gap-1.5 mb-0.5 pl-1">
            <span class="text-[10px] text-text-muted">{fmt(r.timestamp)}</span>
            <button onclick={() => copyText(r.content_preview, mid)} title={t("tool.copyMessage")}
              class="opacity-0 group-hover:opacity-100 transition-opacity text-[11px] leading-none {copiedId === mid ? 'text-success' : 'text-text-muted hover:text-text'}">{copiedId === mid ? "✓" : "⧉"}</button>
          </div>
          <div class="bg-bg-secondary border border-border rounded-2xl rounded-tl-sm px-4 py-2.5 text-text">
            <Markdown content={r.content_preview} {highlight} />
          </div>
        </div>
      </div>

    {:else if r.record_type === "tool_result" && r.tool_use_id && callById.has(r.tool_use_id)}
      <!-- result is shown WITH its call — merged into a normal tool card, or rendered right
           after an Agent's nested work (see the Agent branch). Skip the standalone copy. -->

    {:else if r.record_type === "tool_result"}
      <!-- orphan result: its call isn't in the loaded window — render standalone -->
      {@const k = cardKey(r, "orphan-result")}
      <div class="mx-4">
        <ToolCall record={r} result={r} callRecord={r.tool_use_id ? callById.get(r.tool_use_id) ?? null : null}
          expanded={!!expandedCards[k]} onToggle={() => toggleCard(k)} {highlight} />
      </div>

    {:else if r.tool_name === "AskUserQuestion"}
      {@const k = cardKey(r)}
      <div class="mx-4">
        <ToolCall record={r} result={resultFor(r)}
          expanded={!!expandedCards[k]} onToggle={() => toggleCard(k)} {highlight} />
      </div>

    {:else if isAgentCall(r)}
      <!-- Agent calls are special: call (prompt) → nested work (expand) → return, rendered right
           here after the work (looked up by id), so a parallel agent's result stays with it. -->
      {@const agentResult = resultFor(r)}
      {@const k = cardKey(r)}
      <div class="mx-4">
        <ToolCall record={r} result={null} preHooks={preFor(r)} postHooks={postFor(r)}
          expanded={!!expandedCards[k]} onToggle={() => toggleCard(k)} {highlight} />
      </div>
      {#if matchedAgent}
        <div class="mx-4 ml-8">
          <button onclick={() => toggleAgent(matchedAgent)} disabled={loadingAgent === matchedAgent.agent_id}
            class="w-full text-left px-3 py-2 text-[11px] bg-accent-dim border border-accent/20 rounded-lg hover:border-accent/40 transition-all flex items-center gap-2">
            <span class="w-1.5 h-1.5 rounded-full bg-accent shrink-0"></span>
            <span class="text-accent font-medium">{matchedAgent.agent_type}</span>
            <span class="text-text-secondary truncate">— {matchedAgent.description}</span>
            <span class="text-text-muted ml-auto shrink-0">{loadingAgent === matchedAgent.agent_id ? "…" : expandedAgents[matchedAgent.agent_id] ? t("tool.collapse") : t("tool.expand")}</span>
          </button>
          {#if expandedAgents[matchedAgent.agent_id]}
            {@const agentPage = expandedAgents[matchedAgent.agent_id]}
            <div class="mt-1 pl-2 border-l-2 border-accent/20">
              <!-- A subagent is just another timeline (identical logic, recursively) -->
              <Timeline records={agentPage.records} {session} {subagents} {highlight} />
              {#if agentPage.hasMore}
                <button
                  onclick={() => loadAgentPage(matchedAgent)}
                  disabled={loadingAgent === matchedAgent.agent_id}
                  class="mt-2 w-full rounded-lg border border-border px-3 py-1.5 text-[11px] text-text-secondary hover:bg-bg-hover disabled:opacity-50"
                >{loadingAgent === matchedAgent.agent_id ? t("common.loading") : t("tool.loadMore")}</button>
              {/if}
            </div>
          {/if}
        </div>
      {/if}
      {#if agentResult}
        {@const k = cardKey(agentResult, "agent-result")}
        <div class="mx-4">
          <ToolCall record={agentResult} result={agentResult} callRecord={r}
            expanded={!!expandedCards[k]} onToggle={() => toggleCard(k)} {highlight} />
        </div>
      {/if}

    {:else if r.tool_name}
      {@const k = cardKey(r)}
      <div class="mx-4">
        <ToolCall record={r} result={resultFor(r)} preHooks={preFor(r)} postHooks={postFor(r)}
          expanded={!!expandedCards[k]} onToggle={() => toggleCard(k)} {highlight} />
      </div>

    {:else}
      <div class="flex justify-start">
        <div class="max-w-[75%]">
          {#if r.timestamp}<div class="text-[10px] text-text-muted mb-0.5 pl-1">{fmt(r.timestamp)}</div>{/if}
          <div class="bg-bg-tertiary border border-border rounded-lg px-3 py-2">
            <div class="text-[10px] text-text-muted mb-1">{r.record_type}</div>
            {#if r.content_preview}
              <pre class="whitespace-pre-wrap break-words font-mono text-[11px]">{r.content_preview}</pre>
            {/if}
          </div>
        </div>
      </div>
    {/if}
  {/each}
</div>
