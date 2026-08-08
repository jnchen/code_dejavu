<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import { api } from "$lib/api";
  import { hostLabel, displayPath } from "$lib/hosts";
  import Timeline from "$lib/Timeline.svelte";
  import Markdown from "$lib/Markdown.svelte";
  import { highlightPlain } from "$lib/html";
  import { t } from "$lib/i18n.svelte";
  import { pushToast } from "$lib/toast.svelte";
  import { deferRouteLoad } from "$lib/defer";
  import type {
    InstructionDetail,
    MemoryFile,
    MemoryFrontmatter,
    ProjectContext,
    RuleFile,
    SessionSummary,
    SessionRecord,
    SubagentInfo,
    IndexStatus,
    SessionSearchHit,
    SessionMeta,
    SourceInfo,
    SessionModelInfo,
  } from "$lib/types";

  let sessions = $state<SessionSummary[]>([]);
  let selectedProject = $state<string | null>(null);
  let loading = $state(true);
  let error = $state("");
  let promptCache = $state<Record<string, string>>({});
  let searchQuery = $state("");
  let searching = $state(false);
  let deepSearching = $state(false);
  let searchTimer: ReturnType<typeof setTimeout> | null = null;
  let sessionRequestSeq = 0;
  let indexStatus = $state<IndexStatus | null>(null);
  let indexReady = $derived(indexStatus?.Ready != null);
  let rebuilding = $state(false);
  let exporting = $state(false);
  let exportMenuOpen = $state(false);
  let exportMenuEl = $state<HTMLElement>();
  let notice = $state("");
  let destroyed = false;
  // A first-ever cold start may have no persisted summaries yet. Refresh the empty index view
  // once the full index is ready; never substitute a reduced-fidelity provider result.
  let needsIndexRefresh = false;

  // Which coding agent's sessions we're browsing. ALL_SOURCES is a virtual selection that
  // shows a unified, recency-sorted timeline across every source via the global search index.
  const ALL_SOURCES = "__all__";
  let sources = $state<SourceInfo[]>([]);
  let source = $state<string>("");
  let sessionSources = $derived(sources.filter((s) => s.capabilities.sessions_read));
  let allMode = $derived(source === ALL_SOURCES);

  // App-local per-session metadata (favourite / pinned / tags / note). Stored under the app's own
  // data dir — organising sessions never writes to ~/.claude or other agent files.
  let sessionMeta = $state<Record<string, SessionMeta>>({});
  let onlyFavorites = $state(false);
  let activeTag = $state<string | null>(null);
  let metaPanelOpen = $state(false);
  let metaNoteDraft = $state("");
  let metaTagsDraft = $state("");

  type ArchiveScope = "current" | "all" | "archived";
  const ARCHIVE_SCOPES: ArchiveScope[] = ["current", "all", "archived"];
  let archiveScope = $state<ArchiveScope>("current");

  // Search scope: 对话 / 工具 / 思考. Default 对话 only (avoids noisy hits in collapsed sections).
  const SCOPES = ["content", "tool", "reasoning"];
  // Default to 对话 only; 工具 / 思考 are opt-in via the scope buttons.
  let scopes = $state<string[]>(["content"]);
  const MAX_SEARCH_RESULTS = 200;
  function toggleScope(k: string) {
    scopes = scopes.includes(k) ? scopes.filter((s) => s !== k) : [...scopes, k];
    if (searchQuery.trim()) {
      if (searchTimer) clearTimeout(searchTimer);
      searchTimer = null;
      doSearch();
    }
  }

  let detail = $state<{
    session: SessionSummary;
    records: SessionRecord[];
    hasMore: boolean;
    nextByteOffset: number;
    headByteOffset: number;
    hasEarlier: boolean;
    subagents: SubagentInfo[];
    tailMode: boolean;
  } | null>(null);
  let loadingDetail = $state(false);
  let displayLevel = $state<"content" | "tool" | "debug">("content");
  let detailSearch = $state("");
  let searchHits = $state<SessionSearchHit[]>([]);
  let currentHitIdx = $state(-1);
  let searchingInSession = $state(false);
  let detailSearchTimer: ReturnType<typeof setTimeout> | null = null;
  let detailSearchRequestSeq = 0;
  let detailRequestSeq = 0;
  let contextOpen = $state(false);
  let loadingContext = $state(false);
  let contextError = $state("");
  let projectContext = $state<{
    key: string;
  } & ProjectContext | null>(null);
  let contextFocusedMemory = $state<string | null>(null);
  let contextEditingMemory = $state<MemoryFile | null>(null);
  let contextCreatingMemory = $state(false);
  let contextNewMemoryFilename = $state("");
  let contextMemoryContent = $state("");
  let contextMemoryName = $state("");
  let contextMemoryDesc = $state("");
  let contextMemoryType = $state("feedback");
  let contextEditingArtifactKey = $state<string | null>(null);
  let contextArtifactContent = $state("");
  let contextSaving = $state(false);

  let bottomEl = $state<HTMLElement>();
  let topEl = $state<HTMLElement>();
  let scrollContainer = $state<HTMLElement>();
  let observers: IntersectionObserver[] = [];
  let jumpGuard = false;
  let jumpGuardTimer: ReturnType<typeof setTimeout> | null = null;
  // Bounds auto-loading so a compact (tool/debug) view whose first pages don't fill the
  // viewport can't cascade through the whole 100k-line file. Any real user scroll re-arms it.
  const AUTO_CAP = 15;
  let autoLoads = 0;
  let timelineUserScrollUntil = 0;
  let timelinePointerScrolling = false;
  let ignoreTimelineScrollUntil = 0;

  function markTimelineUserScroll(event: Event) {
    if (!event.isTrusted) return false;
    timelineUserScrollUntil = Date.now() + 750;
    ignoreTimelineScrollUntil = 0; // a real gesture interrupts any smooth/programmatic scroll
    autoLoads = 0;
    // If AUTO_CAP stopped while a sentinel was already intersecting, resetting the counter alone
    // does not produce a new IntersectionObserver transition. Re-check the visible edge after the
    // browser applies this input so keyboard/wheel/scrollbar users can continue immediately.
    setTimeout(() => {
      if (destroyed || loadingDetail || !detail || !scrollContainer) return;
      const atTop = scrollContainer.scrollTop <= 8;
      const distanceFromBottom = scrollContainer.scrollHeight - scrollContainer.scrollTop - scrollContainer.clientHeight;
      if (atTop && detail.hasEarlier) loadEarlier();
      else if (distanceFromBottom <= 8 && detail.hasMore) loadMore();
    }, 0);
    return true;
  }

  function onTimelineUserScroll(event: WheelEvent | TouchEvent) {
    markTimelineUserScroll(event);
  }

  function onTimelineKeyScroll(event: KeyboardEvent) {
    if (event.target !== scrollContainer) return;
    if (!["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "].includes(event.key)) return;
    markTimelineUserScroll(event);
  }

  function onTimelinePointerDown(event: PointerEvent) {
    if (!scrollContainer || event.pointerType !== "mouse") return;
    const rect = scrollContainer.getBoundingClientRect();
    const scrollbarWidth = Math.max(scrollContainer.offsetWidth - scrollContainer.clientWidth, 12);
    if (event.clientX < rect.right - scrollbarWidth) return;
    if (!markTimelineUserScroll(event)) return;
    timelinePointerScrolling = true;
  }

  function onWindowPointerUp() {
    if (!timelinePointerScrolling) return;
    timelinePointerScrolling = false;
    timelineUserScrollUntil = Date.now() + 100;
  }

  function onTimelineScroll(event: Event) {
    const now = Date.now();
    if (
      !event.isTrusted ||
      now < ignoreTimelineScrollUntil ||
      (!timelinePointerScrolling && now > timelineUserScrollUntil)
    ) return;
    autoLoads = 0;
    if (!detail || !scrollContainer) return;
    const distanceFromBottom = scrollContainer.scrollHeight - scrollContainer.scrollTop - scrollContainer.clientHeight;
    detail.tailMode = !detail.hasMore && distanceFromBottom <= 8;
  }

  function markProgrammaticTimelineScroll(durationMs = 100) {
    ignoreTimelineScrollUntil = Math.max(ignoreTimelineScrollUntil, Date.now() + durationMs);
  }

  function clearJumpGuard() {
    if (jumpGuardTimer) clearTimeout(jumpGuardTimer);
    jumpGuardTimer = null;
    jumpGuard = false;
  }

  function armJumpGuard(req: number) {
    if (jumpGuardTimer) clearTimeout(jumpGuardTimer);
    jumpGuard = true;
    const timer = setTimeout(() => {
      if (jumpGuardTimer !== timer) return;
      if (req === detailRequestSeq) jumpGuard = false;
      jumpGuardTimer = null;
    }, 300);
    jumpGuardTimer = timer;
  }

  function cleanupObservers() {
    for (const obs of observers) obs.disconnect();
    observers = [];
  }

  type DetailRequest = {
    seq: number;
    sessionKey: string;
    level: "content" | "tool" | "debug";
  };

  function beginDetailRequest(session: SessionSummary, level: DetailRequest["level"]): DetailRequest {
    clearJumpGuard();
    const request = {
      seq: ++detailRequestSeq,
      sessionKey: sessionCacheKey(session),
      level,
    };
    loadingDetail = true;
    error = "";
    return request;
  }

  function isCurrentDetailRequest(request: DetailRequest): boolean {
    return (
      request.seq === detailRequestSeq &&
      detail != null &&
      sessionCacheKey(detail.session) === request.sessionKey &&
      displayLevel === request.level
    );
  }

  function invalidateDetailRequests() {
    detailRequestSeq++;
    loadingDetail = false;
    clearJumpGuard();
  }

  function finishDetailRequest(request: DetailRequest) {
    if (request.seq === detailRequestSeq) loadingDetail = false;
  }

  function setupObservers() {
    cleanupObservers();
    if (!scrollContainer || !bottomEl || !topEl) return;
    const opts = { root: scrollContainer, threshold: 0.1 };
    const bottomObs = new IntersectionObserver((entries) => {
      if (entries[0]?.isIntersecting && detail?.hasMore && !loadingDetail && !jumpGuard) loadMore();
    }, opts);
    bottomObs.observe(bottomEl);
    const topObs = new IntersectionObserver((entries) => {
      if (entries[0]?.isIntersecting && detail?.hasEarlier && !loadingDetail && !jumpGuard) loadEarlier();
    }, opts);
    topObs.observe(topEl);
    observers = [bottomObs, topObs];
  }


  async function loadSessions() {
    cleanupObservers();
    invalidateDetailRequests();
    const req = ++sessionRequestSeq;
    const project = selectedProject ?? undefined;
    const src = source;
    const archiveSnapshot = archiveScope;
    if (!src) {
      sessions = [];
      loading = false;
      needsIndexRefresh = false;
      return;
    }
    loading = true; detail = null; promptCache = {}; error = "";
    try {
      // ALL_SOURCES has no single provider to `list`, so it always goes through the global
      // index (which already aggregates every source and sorts by recency).
      const isAll = src === ALL_SOURCES;
      // The summary index is the source of truth for every list field. On the first-ever build it
      // can be empty briefly; pollIndexStatus reloads it when complete. Do not replace it with a
      // reduced-fidelity list that drops titles, interaction timestamps or model contexts.
      needsIndexRefresh = !indexReady;
      const result = !isAll && archiveSnapshot === "current"
        ? await api.sessions.browse(src, "current")
        : await api.sessions.search("", isAll ? undefined : src, scopes, archiveSnapshot);
      if (
        req !== sessionRequestSeq ||
        source !== src ||
        archiveScope !== archiveSnapshot ||
        (selectedProject ?? undefined) !== project
      ) return;
      sessions = filterProjectSessions(result, project);
      lazyLoadPrompts(req);
    } catch (e) { if (req === sessionRequestSeq) error = String(e); }
    finally { if (req === sessionRequestSeq) loading = false; }
  }

  async function changeSource(s: string) {
    if (s === source) return;
    if (s !== ALL_SOURCES) {
      const next = sources.find((source) => source.id === s);
      if (next && (!next.available || !next.capabilities.sessions_read)) return;
      if (archiveScope !== "current" && !next?.capabilities.sessions_search) archiveScope = "current";
    }
    source = s;
    selectedProject = null;
    searchQuery = "";
    searching = false;
    detail = null;
    await loadSessions();
  }

  async function lazyLoadPrompts(req = sessionRequestSeq) {
    const visible = sessions.slice(0, 20);
    // Do not issue 20 disk reads at once. A small worker pool keeps scrolling responsive and,
    // unlike Promise.all over the whole list, stops taking new work after navigation invalidates it.
    let next = 0;
    const worker = async () => {
      while (req === sessionRequestSeq && !destroyed) {
        const s = visible[next++];
        if (!s) return;
        const key = sessionCacheKey(s);
        if (promptCache[key] !== undefined || s.first_prompt) continue;
        try {
          const prompt = await api.sessions.getFirstPrompt(s.project, s.session_id, s.archive_name ?? null, s.source);
          if (req !== sessionRequestSeq || destroyed) return;
          promptCache = { ...promptCache, [key]: prompt ?? "" };
        } catch {}
      }
    };
    await Promise.all(Array.from({ length: Math.min(6, visible.length) }, worker));
  }

  function onSearchInput() {
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = null;
    if (!searchQuery.trim()) {
      searching = false;
      loadSessions();
      return;
    }
    searchTimer = setTimeout(doSearch, 400);
  }

  async function doSearch() {
    const q = searchQuery.trim();
    if (!q) {
      await loadSessions();
      return;
    }
    if (searchInputDisabled()) return;
    const req = ++sessionRequestSeq;
    const src = source;
    const project = selectedProject ?? undefined;
    const scopeSnapshot = [...scopes];
    const archiveSnapshot = archiveScope;
    searching = true; loading = true; error = "";
    try {
      const result = await api.sessions.search(q, src === ALL_SOURCES ? undefined : src, scopeSnapshot, archiveSnapshot);
      if (
        req !== sessionRequestSeq ||
        searchQuery.trim() !== q ||
        source !== src ||
        archiveScope !== archiveSnapshot ||
        (selectedProject ?? undefined) !== project ||
        scopes.join("\0") !== scopeSnapshot.join("\0")
      ) return;
      promptCache = {};
      sessions = filterProjectSessions(result, project);
    } catch (e) { if (req === sessionRequestSeq) error = String(e); }
    finally {
      if (req === sessionRequestSeq) {
        searching = false;
        loading = false;
      }
    }
  }

  // Deep full-text search: scans session source files (beyond the 4000-char index preview).
  async function runDeepSearch() {
    const q = searchQuery.trim();
    if (!q || deepSearching || searchInputDisabled()) return;
    const req = ++sessionRequestSeq;
    const src = source;
    const project = selectedProject ?? undefined;
    const archiveSnapshot = archiveScope;
    deepSearching = true; searching = true; loading = true; error = "";
    try {
      const result = await api.sessions.deepSearch(q, src === ALL_SOURCES ? undefined : src, archiveSnapshot);
      if (req !== sessionRequestSeq) return;
      promptCache = {};
      sessions = filterProjectSessions(result, project);
      flashNotice(t("sess.deepNotice", { n: result.length }));
    } catch (e) {
      if (req === sessionRequestSeq) error = String(e);
    } finally {
      if (req === sessionRequestSeq) { deepSearching = false; searching = false; loading = false; }
    }
  }

  async function changeArchiveScope(next: ArchiveScope) {
    if (next === archiveScope) return;
    if (!archiveScopeAvailable(next)) return;
    cleanupObservers();
    invalidateDetailRequests();
    archiveScope = next;
    detail = null;
    promptCache = {};
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = null;
    if (searchQuery.trim()) {
      await doSearch();
    } else {
      searching = false;
      await loadSessions();
    }
  }

  async function openSession(s: SessionSummary) {
    const an = s.archive_name ?? null;
    autoLoads = 0;
    cleanupObservers();
    detailSearchRequestSeq++;
    // Keep the current view level across session switches. Resetting to "content" silently
    // dropped debug-only records (e.g. SessionStart) — you'd switch sessions while in DEBUG and
    // land back in 对话 view, making them look "still missing".
    const level = displayLevel;
    detailSearch = searchQuery;
    searchHits = []; currentHitIdx = -1;
    contextOpen = false;
    metaPanelOpen = false;
    contextError = "";
    projectContext = null;
    contextCreatingMemory = false;
    detail = { session: s, records: [], hasMore: true, nextByteOffset: 0, headByteOffset: 0, hasEarlier: false, subagents: [], tailMode: false };
    const request = beginDetailRequest(s, level);
    try {
      const [result, subagents] = await Promise.all([
        api.sessions.getDetail(s.project, s.session_id, 0, 100, level, an, s.source),
        sourceInfoFor(s.source)?.capabilities.sessions_subagents
          ? api.sessions.listSubagents(s.project, s.session_id, an, s.source)
          : Promise.resolve([]),
      ]);
      if (!isCurrentDetailRequest(request)) return;
      detail = {
        session: s,
        records: result.records,
        hasMore: result.has_more,
        nextByteOffset: result.next_byte_offset,
        headByteOffset: result.start_byte_offset,
        hasEarlier: result.has_earlier,
        subagents,
        tailMode: false,
      };
      error = "";
      await tick();
      if (!isCurrentDetailRequest(request)) return;
      setupObservers();
      if (detailSearch.trim()) doDetailSearch();
    } catch (e) {
      if (isCurrentDetailRequest(request)) error = String(e);
    } finally {
      finishDetailRequest(request);
    }
  }

  async function loadMore() {
    if (!detail || !detail.hasMore || loadingDetail) return;
    if (autoLoads >= AUTO_CAP) return; // wait for a real user scroll to continue
    autoLoads++;
    const session = detail.session;
    const nextByteOffset = detail.nextByteOffset;
    const level = displayLevel;
    const request = beginDetailRequest(session, level);
    try {
      const result = await api.sessions.getDetail(
        session.project, session.session_id, nextByteOffset, 100, level, session.archive_name, session.source
      );
      if (!isCurrentDetailRequest(request) || !detail || detail.nextByteOffset !== nextByteOffset) return;
      detail.records = [...detail.records, ...result.records];
      detail.hasMore = result.has_more;
      detail.nextByteOffset = result.next_byte_offset;
      error = "";
    } catch (e) {
      if (isCurrentDetailRequest(request)) error = String(e);
    } finally {
      finishDetailRequest(request);
    }
  }

  async function jumpToLatest() {
    if (!detail || loadingDetail) return;
    autoLoads = 0;
    const session = detail.session;
    const level = displayLevel;
    const request = beginDetailRequest(session, level);
    try {
      const result = await api.sessions.getTail(
        session.project, session.session_id, 100, level, session.archive_name, session.source
      );
      if (!isCurrentDetailRequest(request) || !detail) return;
      detail.records = result.records;
      detail.hasMore = result.has_more;
      detail.hasEarlier = result.has_earlier;
      detail.headByteOffset = result.start_byte_offset;
      detail.nextByteOffset = result.next_byte_offset;
      detail.tailMode = true; // anchored at the end — keep it that way across level switches
      error = "";
      armJumpGuard(request.seq);
      await tick();
      if (!isCurrentDetailRequest(request)) return;
      markProgrammaticTimelineScroll(600);
      bottomEl?.scrollIntoView({ behavior: "smooth" });
    } catch (e) {
      if (isCurrentDetailRequest(request)) error = String(e);
    } finally {
      finishDetailRequest(request);
    }
  }

  async function loadEarlier() {
    if (!detail || !detail.hasEarlier || loadingDetail) return;
    if (autoLoads >= AUTO_CAP) return; // wait for a real user scroll to continue
    autoLoads++;
    const prevHeight = scrollContainer?.scrollHeight ?? 0;
    const session = detail.session;
    const beforeByteOffset = detail.headByteOffset;
    const wasTailMode = detail.tailMode;
    const level = displayLevel;
    const request = beginDetailRequest(session, level);
    try {
      const result = await api.sessions.getBefore(
        session.project, session.session_id, beforeByteOffset, 100, level, session.archive_name, session.source
      );
      if (!isCurrentDetailRequest(request) || !detail || detail.headByteOffset !== beforeByteOffset) return;
      detail.records = [...result.records, ...detail.records];
      detail.headByteOffset = result.start_byte_offset;
      detail.hasEarlier = result.has_earlier;
      // Compact tail views may auto-load an older page merely to fill the viewport. Preserve the
      // explicit tail anchor unless a trusted user scroll already cleared it.
      detail.tailMode = wasTailMode;
      error = "";
      await tick();
      if (!isCurrentDetailRequest(request)) return;
      if (scrollContainer) {
        const newHeight = scrollContainer.scrollHeight;
        markProgrammaticTimelineScroll();
        scrollContainer.scrollTop += newHeight - prevHeight;
      }
    } catch (e) {
      if (isCurrentDetailRequest(request)) error = String(e);
    } finally {
      finishDetailRequest(request);
    }
  }

  async function changeLevel(level: "content" | "tool" | "debug") {
    if (!detail || level === displayLevel || loadingDetail) return;
    const session = detail.session;
    const atTail = detail.tailMode;
    const previousLevel = displayLevel;
    autoLoads = 0;
    displayLevel = level;
    const request = beginDetailRequest(session, level);
    // If the user is anchored at the end (e.g. clicked 最新), switching level must KEEP them
    // at the end — re-load the tail at the new level instead of snapping back to byte 0.
    try {
      const result = atTail
        ? await api.sessions.getTail(
            session.project, session.session_id, 100, level, session.archive_name, session.source
          )
        : await api.sessions.getDetail(
            session.project, session.session_id, 0, 100, level, session.archive_name, session.source
          );
      if (!isCurrentDetailRequest(request) || !detail) return;
      detail.records = result.records;
      detail.hasMore = result.has_more;
      detail.nextByteOffset = result.next_byte_offset;
      detail.headByteOffset = result.start_byte_offset;
      detail.hasEarlier = result.has_earlier;
      detail.tailMode = atTail;
      error = "";
      await tick();
      if (!isCurrentDetailRequest(request)) return;
      if (atTail) {
        markProgrammaticTimelineScroll();
        bottomEl?.scrollIntoView();
      } else if (scrollContainer) {
        // This page starts at byte 0, where the header records live.
        markProgrammaticTimelineScroll();
        scrollContainer.scrollTop = 0;
      }
    } catch (e) {
      if (isCurrentDetailRequest(request)) {
        displayLevel = previousLevel;
        error = String(e);
      }
    } finally {
      finishDetailRequest(request);
    }
  }

  function closeDetail() {
    cleanupObservers();
    invalidateDetailRequests();
    detailSearchRequestSeq++;
    if (detailSearchTimer) clearTimeout(detailSearchTimer);
    detailSearchTimer = null;
    searchingInSession = false;
    detail = null;
    detailSearch = "";
    searchHits = [];
    currentHitIdx = -1;
    error = "";
  }

  function onDetailSearchInput() {
    if (detailSearchTimer) clearTimeout(detailSearchTimer);
    detailSearchTimer = null;
    if (!detailSearch.trim() || !detail) {
      detailSearchRequestSeq++;
      searchingInSession = false;
      searchHits = []; currentHitIdx = -1;
      return;
    }
    detailSearchTimer = setTimeout(doDetailSearch, 400);
  }

  async function doDetailSearch() {
    if (!detail || !detailSearch.trim()) return;
    const req = ++detailSearchRequestSeq;
    const session = detail.session;
    const q = detailSearch.trim();
    const detailGeneration = detailRequestSeq;
    searchingInSession = true;
    error = "";
    try {
      const hits = await api.sessions.searchInSession(
        session.project, session.session_id, q, session.archive_name, session.source
      );
      if (
        req !== detailSearchRequestSeq ||
        !detail ||
        sessionCacheKey(detail.session) !== sessionCacheKey(session) ||
        detailSearch.trim() !== q
      ) return;
      searchHits = hits;
      currentHitIdx = searchHits.length > 0 ? 0 : -1;
      // Do not let an older search completion override a newer explicit detail action (for
      // example, the user clicking 最新 while the search was still running).
      if (currentHitIdx >= 0 && detailGeneration === detailRequestSeq) jumpToHit(0);
    } catch (e) {
      if (
        req === detailSearchRequestSeq &&
        detail &&
        sessionCacheKey(detail.session) === sessionCacheKey(session) &&
        detailSearch.trim() === q &&
        detailGeneration === detailRequestSeq
      ) error = String(e);
    }
    finally { if (req === detailSearchRequestSeq) searchingInSession = false; }
  }

  async function jumpToHit(idx: number) {
    if (!detail || idx < 0 || idx >= searchHits.length) return;
    currentHitIdx = idx;
    autoLoads = 0;
    const hit = searchHits[idx];
    const session = detail.session;
    const q = detailSearch.trim();
    const searchRequest = detailSearchRequestSeq;
    const previousLevel = displayLevel;
    // Tool I/O is hidden in 对话 view — raise the level so the matched record is actually visible.
    const level = (hit.record_type === "tool" || hit.record_type === "tool_result") && displayLevel === "content"
      ? "tool"
      : displayLevel;
    displayLevel = level;
    const request = beginDetailRequest(session, level);
    try {
      const startOffset = hit.byte_offset;
      const result = await api.sessions.getDetail(
        session.project, session.session_id, startOffset, 100, level, session.archive_name, session.source
      );
      if (!isCurrentDetailRequest(request) || !detail) return;
      if (
        detailSearchRequestSeq !== searchRequest ||
        detailSearch.trim() !== q ||
        searchHits[idx]?.byte_offset !== hit.byte_offset
      ) {
        displayLevel = previousLevel;
        return;
      }
      detail.records = result.records;
      detail.hasMore = result.has_more;
      detail.nextByteOffset = result.next_byte_offset;
      detail.headByteOffset = result.start_byte_offset;
      detail.hasEarlier = result.has_earlier;
      detail.tailMode = false;
      error = "";
      armJumpGuard(request.seq);
      await tick();
      if (!isCurrentDetailRequest(request)) return;
      if (scrollContainer) {
        markProgrammaticTimelineScroll();
        scrollContainer.scrollTop = 0; // hit is at the window top — show it
      }
      setupObservers();
    } catch (e) {
      if (isCurrentDetailRequest(request)) {
        displayLevel = previousLevel;
        error = String(e);
      }
    } finally {
      finishDetailRequest(request);
    }
  }


  let activeHighlight = $derived(detailSearch || searchQuery);

  function formatSize(bytes: number): string {
    if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + " MB";
    if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
    return bytes + " B";
  }

  function formatTime(ts: string | null): string {
    return ts ?? "";
  }

  function createdTime(session: SessionSummary): string {
    return formatTime(session.created_at ?? null);
  }

  function updatedTime(session: SessionSummary): string {
    return formatTime(session.updated_at ?? session.timestamp ?? null);
  }

  function sessionTitle(session: SessionSummary, prompt?: string | null): string {
    return session.agent_title?.trim() || prompt?.trim() || t("sess.noPrompt");
  }

  function shouldShowPromptSubtitle(session: SessionSummary, prompt?: string | null): boolean {
    const title = session.agent_title?.trim();
    if (!title) return false;
    return prompt == null || prompt.trim() !== title;
  }

  function shortSessionId(session: SessionSummary): string {
    return session.session_id.slice(0, 8);
  }

  async function copySessionId(session: SessionSummary) {
    try {
      await navigator.clipboard.writeText(session.session_id);
      flashNotice(t("sess.idCopied"));
    } catch (e) {
      error = String(e);
    }
  }

  function currentSourceInfo() {
    return sourceInfoFor(source);
  }

  function sourceInfoFor(sourceId?: string | null) {
    return sourceId ? sources.find((s) => s.id === sourceId) : undefined;
  }

  function sourceSessionCount(sourceId: string): number {
    if (source === sourceId) return sessions.length;
    return 0;
  }

  function canResumeSession(session: SessionSummary): boolean {
    return sourceInfoFor(session.source)?.capabilities.sessions_resume ?? false;
  }

  function sourceLabel(session: SessionSummary): string {
    return sourceInfoFor(session.source)?.display_name ?? session.source ?? t("sess.unknownSource");
  }

  function sessionCacheKey(session: SessionSummary): string {
    return `${session.source ?? ""}\0${session.archive_name ?? ""}\0${session.project}\0${session.session_id}`;
  }

  function filterProjectSessions(result: SessionSummary[], project?: string): SessionSummary[] {
    if (!project) return result;
    return result.filter((session) => session.project === project || session.project_path === project);
  }

  function metaKey(s: SessionSummary): string {
    return `${s.source ?? ""}::${s.session_id}`;
  }
  function metaFor(s: SessionSummary): SessionMeta {
    return sessionMeta[metaKey(s)] ?? { favorite: false, pinned: false, tags: [], note: "" };
  }
  async function updateMeta(s: SessionSummary, patch: Partial<SessionMeta>) {
    const key = metaKey(s);
    const next = { ...metaFor(s), ...patch };
    sessionMeta = { ...sessionMeta, [key]: next };
    try {
      await api.sessionMeta.set(key, next);
    } catch (e) {
      error = String(e);
    }
  }
  function toggleFavorite(s: SessionSummary) {
    updateMeta(s, { favorite: !metaFor(s).favorite });
  }
  function togglePinned(s: SessionSummary) {
    updateMeta(s, { pinned: !metaFor(s).pinned });
  }

  // All tags currently in use, for the tag filter chips.
  let allTags = $derived.by(() => {
    const set = new Set<string>();
    for (const v of Object.values(sessionMeta)) for (const t of v.tags) set.add(t);
    return [...set].sort();
  });
  // The list actually rendered: favourite/tag filters applied, pinned floated to the top while
  // preserving the backend's recency order otherwise.
  let visibleSessions = $derived.by(() => {
    let list = sessions;
    if (onlyFavorites) list = list.filter((s) => metaFor(s).favorite);
    if (activeTag) list = list.filter((s) => metaFor(s).tags.includes(activeTag!));
    return [...list].sort((a, b) => (metaFor(b).pinned ? 1 : 0) - (metaFor(a).pinned ? 1 : 0));
  });

  // Incremental render window: only the first `renderLimit` cards are ever in the DOM, growing as
  // the user scrolls toward the bottom. A multi-thousand-session list otherwise mounts every card
  // at once (huge DOM, janky scroll). The window resets to the top whenever the list / filters
  // change so a new search or source switch always starts from the most relevant results.
  const SESSION_PAGE = 60;
  let renderLimit = $state(SESSION_PAGE);
  let renderedSessions = $derived(visibleSessions.slice(0, renderLimit));
  $effect(() => {
    // Register the inputs that produce a fresh list so the window snaps back to the top.
    void sessions;
    void onlyFavorites;
    void activeTag;
    renderLimit = SESSION_PAGE;
  });
  function onListScroll(e: Event) {
    const el = e.currentTarget as HTMLElement;
    if (
      el.scrollTop + el.clientHeight >= el.scrollHeight - 600 &&
      renderLimit < visibleSessions.length
    ) {
      renderLimit = Math.min(renderLimit + SESSION_PAGE, visibleSessions.length);
    }
  }

  function openMetaPanel() {
    if (!detail) return;
    const m = metaFor(detail.session);
    metaNoteDraft = m.note;
    metaTagsDraft = m.tags.join(", ");
    metaPanelOpen = true;
  }
  async function saveMetaPanel() {
    if (!detail) return;
    const tags = metaTagsDraft.split(",").map((t) => t.trim()).filter(Boolean);
    await updateMeta(detail.session, { note: metaNoteDraft, tags });
    metaPanelOpen = false;
  }

  // In ALL mode the global index covers every source, so search is capable regardless of any
  // single provider's flag (it does require the index to be ready, handled by callers).
  function searchCapable(): boolean {
    if (allMode) return true;
    return currentSourceInfo()?.capabilities.sessions_search ?? false;
  }

  function archiveScopeAvailable(scope: ArchiveScope): boolean {
    if (allMode) return indexReady; // every scope in ALL mode is served by the index
    if (scope === "current") return true;
    return indexReady && searchCapable();
  }

  function searchInputDisabled(): boolean {
    return !indexReady || !searchCapable();
  }

  function searchPlaceholder(): string {
    if (!searchCapable()) return t("sess.searchDisabled");
    if (!indexReady) return t("sess.indexBuilding");
    if (searching) return t("sess.searching");
    if (archiveScope === "all") return t("sess.searchAll");
    if (archiveScope === "archived") return t("sess.searchArchived");
    return t("sess.searchCurrent");
  }

  function emptySessionsLabel(): string {
    if (searchQuery.trim()) return t("sess.emptyNoMatch");
    if (archiveScope === "all") return t("sess.emptyAll");
    if (archiveScope === "archived") return t("sess.emptyArchived");
    return t("sess.emptyCurrent");
  }

  function contextKey(session: SessionSummary): string {
    return `${session.source ?? ""}\0${session.project}\0${session.project_path}`;
  }

  async function toggleProjectContext() {
    contextOpen = !contextOpen;
    if (contextOpen) await loadProjectContext();
  }

  async function loadProjectContext() {
    if (!detail) return;
    const session = detail.session;
    const key = contextKey(session);
    if (projectContext?.key === key) return;
    loadingContext = true;
    contextError = "";
    contextFocusedMemory = null;
    contextEditingMemory = null;
    contextCreatingMemory = false;
    contextEditingArtifactKey = null;
    try {
      if (!session.source) throw new Error(t("ctx.noSource"));
      const context = await api.instructions.projectContext(session.source, session.project, session.project_path);
      projectContext = { ...context, key };
    } catch (e) {
      contextError = String(e);
    } finally {
      loadingContext = false;
    }
  }

  function projectContextCount(): number {
    if (!projectContext) return 0;
    return (
      projectContext.instructions.filter((item) => item.exists).length +
      projectContext.configs.filter((item) => item.exists).length +
      projectContext.rules.length +
      projectContext.memories.length
    );
  }

  function projectContextExpectedCount(): number {
    if (!projectContext) return 0;
    return (
      projectContext.instructions.length +
      projectContext.configs.length +
      projectContext.rules.length +
      projectContext.memories.length
    );
  }

  function artifactStatusLabel(item: InstructionDetail): string {
    return item.exists ? t("ctx.found") : t("ctx.notCreated");
  }

  function artifactActionLabel(item: InstructionDetail): string {
    return item.exists ? t("common.edit") : t("ctx.create");
  }

  function canWriteMemory(sourceId?: string | null): boolean {
    return sourceInfoFor(sourceId)?.capabilities.memory_write ?? false;
  }

  function artifactKey(item: InstructionDetail): string {
    return `${item.source}\0${item.path}`;
  }

  function startContextArtifactEdit(item: InstructionDetail) {
    if (!item.editable) return;
    contextEditingArtifactKey = artifactKey(item);
    contextArtifactContent = item.content;
  }

  async function saveContextArtifact(item: InstructionDetail) {
    if (!detail || !item.editable) return;
    contextSaving = true;
    contextError = "";
    try {
      await api.instructions.save(item.source, item.path, contextArtifactContent);
      projectContext = null;
      await loadProjectContext();
      contextEditingArtifactKey = null;
    } catch (e) {
      contextError = String(e);
    } finally {
      contextSaving = false;
    }
  }

  function startContextMemoryEdit(memory: MemoryFile) {
    if (!canWriteMemory(memory.source)) return;
    contextCreatingMemory = false;
    contextEditingMemory = memory;
    contextMemoryContent = memory.content;
    contextMemoryName = memory.frontmatter?.name ?? "";
    contextMemoryDesc = memory.frontmatter?.description ?? "";
    contextMemoryType = memory.frontmatter?.type ?? "feedback";
  }

  function nextContextMemoryFilename(): string {
    const used = new Set(projectContext?.memories.map((memory) => memory.filename.toLowerCase()) ?? []);
    let i = 1;
    while (used.has(`project-memory-${i}.md`)) i += 1;
    return `project-memory-${i}.md`;
  }

  function normalizeMemoryFilename(value: string): string {
    const clean = value.trim().replace(/[\\\/]/g, "-").replace(/^\.+/, "");
    const filename = clean || nextContextMemoryFilename();
    return filename.toLowerCase().endsWith(".md") ? filename : `${filename}.md`;
  }

  function startContextMemoryCreate() {
    if (!projectContext?.memory_project || !projectContext.memory_status.writable) return;
    contextEditingMemory = null;
    contextCreatingMemory = true;
    contextNewMemoryFilename = nextContextMemoryFilename();
    contextMemoryContent = "";
    contextMemoryName = "";
    contextMemoryDesc = "";
    contextMemoryType = "project";
  }

  async function saveContextMemory() {
    if (!contextEditingMemory || !canWriteMemory(contextEditingMemory.source)) return;
    contextSaving = true;
    contextError = "";
    try {
      const fm: MemoryFrontmatter = {
        name: contextMemoryName || null,
        description: contextMemoryDesc || null,
        type: contextMemoryType || null,
        metadata: null,
      };
      await api.memories.save(
        contextEditingMemory.project,
        contextEditingMemory.filename,
        fm,
        contextMemoryContent,
        contextEditingMemory.source
      );
      projectContext = null;
      await loadProjectContext();
      contextFocusedMemory = contextEditingMemory.filename;
      contextEditingMemory = null;
    } catch (e) {
      contextError = String(e);
    } finally {
      contextSaving = false;
    }
  }

  async function saveContextNewMemory() {
    if (!projectContext?.memory_project || !projectContext.memory_status.writable) return;
    contextSaving = true;
    contextError = "";
    try {
      const filename = normalizeMemoryFilename(contextNewMemoryFilename);
      const fm: MemoryFrontmatter = {
        name: contextMemoryName || null,
        description: contextMemoryDesc || null,
        type: contextMemoryType || null,
        metadata: null,
      };
      await api.memories.create(
        projectContext.memory_project.slug,
        filename,
        fm,
        contextMemoryContent,
        projectContext.source
      );
      projectContext = null;
      await loadProjectContext();
      contextFocusedMemory = filename;
      contextCreatingMemory = false;
    } catch (e) {
      contextError = String(e);
    } finally {
      contextSaving = false;
    }
  }

  function navigateContextMemory(href: string) {
    if (!projectContext) return;
    const target = decodeURIComponent(href.replace(/^\.\//, "").split(/[?#]/)[0]);
    const base = target.replace(/\.md$/i, "");
    const found = projectContext.memories.find(
      (memory) =>
        memory.filename === target ||
        memory.filename === `${base}.md` ||
        memory.frontmatter?.name === base
    );
    if (found) {
      contextFocusedMemory = found.filename;
      contextEditingMemory = null;
    } else {
      contextError = t("ctx.linkNotFound", { href });
    }
  }

  async function toggleContextRule(rule: RuleFile) {
    if (!rule.toggleable || !sourceInfoFor(rule.source)?.capabilities.rules_write) return;
    contextSaving = true;
    contextError = "";
    try {
      await api.rules.toggle(rule.category, rule.filename, !rule.enabled, rule.source);
      projectContext = null;
      await loadProjectContext();
    } catch (e) {
      contextError = String(e);
    } finally {
      contextSaving = false;
    }
  }


  function modelContextLabel(context: SessionModelInfo): string {
    return [context.provider, context.model, context.thinking_level]
      .filter((part): part is string => Boolean(part?.trim()))
      .join(" / ");
  }

  function modelContextsTitle(session: SessionSummary): string {
    const labels = (session.model_contexts ?? []).map(modelContextLabel).filter(Boolean);
    if (labels.length === 0) return "";
    const visible = labels.slice(0, 8);
    if (labels.length > visible.length) visible.push(`... +${labels.length - visible.length}`);
    return `Models\n${visible.join("\n")}`;
  }

  function hasModelContexts(session: SessionSummary): boolean {
    return modelContextsTitle(session).length > 0;
  }

  async function pollIndexStatus() {
    while (!destroyed) {
      try {
        const status = await api.sessions.getIndexStatus();
        if (destroyed) return;
        indexStatus = status;
        if (indexStatus?.Ready) {
          // If the initial metadata-only fallback is still in flight, let it settle first and
          // then replace it with the completed index result. This avoids competing list requests.
          while (!destroyed && loading) {
            await new Promise(r => setTimeout(r, 50));
          }
          if (!destroyed && needsIndexRefresh && !detail) {
            needsIndexRefresh = false;
            await loadSessions();
          }
          break;
        }
      } catch {}
      await new Promise(r => setTimeout(r, 500));
    }
  }

  async function rebuildIndex() {
    if (rebuilding) return;
    rebuilding = true;
    try {
      indexStatus = await api.sessions.rebuildIndex();
      if (searchQuery.trim()) doSearch();
      else loadSessions();
    } catch (e) {
      error = String(e);
    } finally {
      rebuilding = false;
    }
  }

  function flashNotice(msg: string) {
    notice = msg;
    setTimeout(() => { if (notice === msg) notice = ""; }, 5000);
  }

  // Pull the whole session (all pages at the current view level) for export/copy.
  async function fetchAllDetailRecords(): Promise<SessionRecord[]> {
    if (!detail) return [];
    const s = detail.session;
    const all: SessionRecord[] = [];
    let offset = 0;
    for (let guard = 0; guard < 5000; guard++) {
      const page = await api.sessions.getDetail(
        s.project, s.session_id, offset, 500, displayLevel, s.archive_name, s.source
      );
      all.push(...page.records);
      if (!page.has_more) break;
      offset = page.next_byte_offset;
    }
    return all;
  }

  function detailToMarkdown(records: SessionRecord[]): string {
    if (!detail) return "";
    const s = detail.session;
    const out: string[] = [];
    out.push(`# ${s.agent_title || s.first_prompt || s.project_path || s.session_id}`, "");
    out.push(`- ${t("sess.mdSource")}: ${sourceLabel(s)}`);
    out.push(`- ${t("sess.mdProject")}: ${s.project_path}`);
    if (s.agent_title) out.push(`- ${t("sess.mdAgentTitle")}: ${s.agent_title}`);
    if (s.created_at) out.push(`- ${t("sess.createdAt")}: ${s.created_at}`);
    if (s.updated_at ?? s.timestamp) out.push(`- ${t("sess.updatedAt")}: ${s.updated_at ?? s.timestamp}`);
    out.push("");
    for (const r of records) {
      if (r.record_type === "user") {
        out.push("## 🧑 User", "", r.content_preview, "");
      } else if (r.record_type === "assistant") {
        if (r.tool_name) {
          out.push(`### 🔧 ${r.tool_name}`, "", "```json", JSON.stringify(r.tool_input ?? {}, null, 2), "```", "");
        } else if (r.content_preview) {
          out.push("## 🤖 Assistant", "", r.content_preview, "");
        }
      } else if (r.record_type === "thinking") {
        out.push("> 💭 " + r.content_preview.replace(/\n/g, "\n> "), "");
      } else if (r.record_type === "tool_result") {
        out.push("```", r.content_preview, "```", "");
      } else if (r.content_preview) {
        out.push(`_${r.record_type}_: ${r.content_preview}`, "");
      }
    }
    return out.join("\n");
  }

  async function copyDetail() {
    if (!detail || exporting) return;
    exporting = true;
    try {
      const md = await formatDetailOffThread("md", await fetchAllDetailRecords());
      await navigator.clipboard.writeText(md);
      flashNotice(t("sess.copied"));
    } catch (e) {
      error = t("sess.copyFailed") + String(e);
    } finally {
      exporting = false;
    }
  }

  function detailToJson(records: SessionRecord[]): string {
    if (!detail) return "[]";
    const s = detail.session;
    return JSON.stringify(
      {
        source: s.source ?? null,
        session_id: s.session_id,
        project_path: s.project_path,
        agent_title: s.agent_title ?? null,
        created_at: s.created_at ?? null,
        updated_at: s.updated_at ?? s.timestamp ?? null,
        timestamp: s.timestamp ?? null,
        first_prompt: s.first_prompt ?? null,
        records,
      },
      null,
      2
    );
  }

  function escapeHtml(s: string): string {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  function detailToHtml(records: SessionRecord[]): string {
    if (!detail) return "";
    const s = detail.session;
    const title = s.agent_title || s.first_prompt || s.project_path || s.session_id;
    const rows = records
      .map((r) => {
        const ts = r.timestamp ? `<div class="ts">${escapeHtml(r.timestamp)}</div>` : "";
        if (r.record_type === "user")
          return `<div class="msg user"><div class="role">🧑 User</div>${ts}<pre>${escapeHtml(r.content_preview)}</pre></div>`;
        if (r.record_type === "assistant" && r.tool_name)
          return `<div class="msg tool"><div class="role">🔧 ${escapeHtml(r.tool_name)}</div><pre>${escapeHtml(JSON.stringify(r.tool_input ?? {}, null, 2))}</pre></div>`;
        if (r.record_type === "assistant")
          return `<div class="msg assistant"><div class="role">🤖 Assistant</div>${ts}<pre>${escapeHtml(r.content_preview)}</pre></div>`;
        if (r.record_type === "thinking")
          return `<div class="msg think"><div class="role">💭 Thinking</div><pre>${escapeHtml(r.content_preview)}</pre></div>`;
        if (r.record_type === "tool_result")
          return `<div class="msg result"><div class="role">↳ Result</div><pre>${escapeHtml(r.content_preview)}</pre></div>`;
        if (r.content_preview)
          return `<div class="msg other"><div class="role">${escapeHtml(r.record_type)}</div><pre>${escapeHtml(r.content_preview)}</pre></div>`;
        return "";
      })
      .join("\n");
    return `<!doctype html><html lang="zh"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${escapeHtml(title)}</title>
<style>
body{font-family:system-ui,-apple-system,"Microsoft YaHei",sans-serif;max-width:860px;margin:24px auto;padding:0 16px;color:#212529;background:#fff;line-height:1.6}
h1{font-size:18px}.meta{color:#868e96;font-size:12px;margin-bottom:20px}
.msg{margin:14px 0;padding:10px 14px;border-radius:10px;border:1px solid #dee2e6}
.user{background:#eef0fb}.assistant{background:#f8f9fa}.tool,.result{background:#f1f3f5}.think{background:#fff8e1;color:#495057}
.role{font-weight:600;font-size:12px;margin-bottom:4px}.ts{color:#adb5bd;font-size:11px;margin-bottom:4px}
pre{white-space:pre-wrap;word-break:break-word;margin:0;font-family:ui-monospace,monospace;font-size:12px}
</style></head>
<body>
<h1>${escapeHtml(title)}</h1>
<div class="meta">${escapeHtml(sourceLabel(s))} · ${escapeHtml(s.project_path)}${updatedTime(s) ? " · " + escapeHtml(t("sess.updatedAt")) + " " + escapeHtml(updatedTime(s)) : ""}</div>
${rows}
</body></html>`;
  }

  // Formatting a large export (JSON stringify, HTML escaping, Markdown assembly) can itself be
  // a long main-thread task after all pages have been fetched. Keep it in a short-lived worker.
  function formatDetailOffThread(format: "md" | "json" | "html", records: SessionRecord[]): Promise<string> {
    if (!detail || typeof Worker === "undefined") {
      return Promise.resolve(format === "md" ? detailToMarkdown(records) : format === "json" ? detailToJson(records) : detailToHtml(records));
    }
    return new Promise((resolve, reject) => {
      let worker: Worker;
      try {
        worker = new Worker(new URL("../../lib/sessionExport.worker.ts", import.meta.url), { type: "module" });
      } catch (e) {
        reject(e);
        return;
      }
      worker.onmessage = (event: MessageEvent<{ ok: boolean; content?: string; error?: string }>) => {
        worker.terminate();
        if (event.data.ok) resolve(event.data.content ?? "");
        else reject(new Error(event.data.error ?? "export worker failed"));
      };
      worker.onerror = (event) => {
        worker.terminate();
        reject(event.error ?? new Error(event.message));
      };
      const s = detail!.session;
      worker.postMessage({
        format,
        session: {
          source: s.source ?? null,
          session_id: s.session_id,
          project_path: s.project_path,
          agent_title: s.agent_title ?? null,
          created_at: s.created_at ?? null,
          updated_at: s.updated_at ?? s.timestamp ?? null,
          timestamp: s.timestamp ?? null,
          first_prompt: s.first_prompt ?? null,
        },
        records,
        sourceLabel: sourceLabel(s),
        labels: {
          source: t("sess.mdSource"),
          project: t("sess.mdProject"),
          time: t("sess.mdTime"),
          agentTitle: t("sess.mdAgentTitle"),
          created: t("sess.createdAt"),
          updated: t("sess.updatedAt"),
        },
      });
    });
  }

  async function exportAs(format: "md" | "json" | "html") {
    if (!detail || exporting) return;
    exportMenuOpen = false;
    exporting = true;
    try {
      const records = await fetchAllDetailRecords();
      const base =
        (detail.session.first_prompt || detail.session.session_id).slice(0, 40).trim() || "session";
      let content: string;
      let ext: string;
      if (format === "json") {
        content = await formatDetailOffThread("json", records);
        ext = "json";
      } else if (format === "html") {
        content = await formatDetailOffThread("html", records);
        ext = "html";
      } else {
        content = await formatDetailOffThread("md", records);
        ext = "md";
      }
      const path = await api.shell.saveExport(`${base}.${ext}`, content);
      await api.shell.revealPath(path);
      flashNotice(t("sess.exportedTo") + path);
    } catch (e) {
      error = t("sess.exportFailed") + String(e);
    } finally {
      exporting = false;
    }
  }

  function onWindowPointerDown(event: PointerEvent) {
    if (exportMenuOpen && exportMenuEl && !exportMenuEl.contains(event.target as Node)) {
      exportMenuOpen = false;
    }
  }

  onMount(() => deferRouteLoad(async () => {
    if (destroyed) return;
    pollIndexStatus();
    api.sessionMeta.list()
      .then((m) => { if (!destroyed) sessionMeta = m; })
      .catch(() => { if (!destroyed) pushToast(t("toast.metaLoadFailed")); });
    const availableSources = await api.sessions.listSources().catch(() => []);
    if (destroyed) return;
    sources = availableSources;
    source =
      sessionSources.find((s) => s.id === source && s.available)?.id ??
      sessionSources.find((s) => s.available)?.id ??
      sessionSources[0]?.id ??
      "";
    // A persisted index is loaded near-instantly on launch; read its status once up front so the
    // first list load can already take the fast index path instead of a one-off disk scan.
    const status = await api.sessions.getIndexStatus().catch(() => indexStatus);
    if (destroyed) return;
    indexStatus = status;
    await loadSessions();
  }));

  onDestroy(() => {
    destroyed = true;
    sessionRequestSeq++;
    detailSearchRequestSeq++;
    invalidateDetailRequests();
    cleanupObservers();
    if (searchTimer) clearTimeout(searchTimer);
    if (detailSearchTimer) clearTimeout(detailSearchTimer);
    searchTimer = null;
    detailSearchTimer = null;
  });
</script>

<svelte:window onpointerdown={onWindowPointerDown} onpointerup={onWindowPointerUp} onpointercancel={onWindowPointerUp} />

<div class="p-6 h-full flex flex-col">
  <div class="flex items-center gap-3 mb-4 shrink-0">
    <h2 class="text-lg font-semibold shrink-0">{t("sess.title")}</h2>
    {#if !detail && sessionSources.length > 1}
      <div class="flex rounded-lg border border-border overflow-hidden shrink-0">
        <button
          onclick={() => changeSource(ALL_SOURCES)}
          disabled={!indexReady}
          title={indexReady ? t("sess.allTimeline") : t("sess.indexBuildingDisabled")}
          class="text-[11px] px-2.5 py-1 transition-colors
            {allMode ? 'bg-accent text-white' : indexReady ? 'hover:bg-bg-hover' : 'text-text-muted opacity-50 cursor-not-allowed'}"
        >{t("sess.all")}</button>
        {#each sessionSources as s}
          <button
            onclick={() => changeSource(s.id)}
            disabled={!s.available || !s.capabilities.sessions_read}
            title={s.available ? t("sess.sourceListTitle", { name: s.display_name, n: sourceSessionCount(s.id) }) : t("sess.sourceNoData", { name: s.display_name })}
            class="text-[11px] px-2.5 py-1 border-l border-border first:border-l-0 transition-colors
              {source === s.id ? 'bg-accent text-white' : s.available && s.capabilities.sessions_read ? 'hover:bg-bg-hover' : 'text-text-muted opacity-50 cursor-not-allowed'}"
          >
            {s.display_name}
            {#if !s.available}
              <span class="ml-1 text-[9px]">{t("sess.notFound")}</span>
            {/if}
          </button>
        {/each}
      </div>
    {/if}
    {#if !detail}
      <div class="flex rounded-lg border border-border overflow-hidden shrink-0" aria-label={t("sess.scopeAria")}>
        {#each ARCHIVE_SCOPES as item}
          {@const available = archiveScopeAvailable(item)}
          <button
            onclick={() => changeArchiveScope(item)}
            disabled={!available}
            title={available ? t("sess.scope." + item + "Title") : t("sess.scopeNeedsIndex", { label: t("sess.scope." + item) })}
            class="text-[11px] px-2.5 py-1 border-l border-border first:border-l-0 transition-colors
              {archiveScope === item ? 'bg-accent text-white' : available ? 'hover:bg-bg-hover' : 'text-text-muted opacity-50 cursor-not-allowed'}"
          >
            {t("sess.scope." + item)}
          </button>
        {/each}
      </div>
      <div class="flex-1 relative">
        <input
          bind:value={searchQuery}
          oninput={onSearchInput}
          disabled={searchInputDisabled()}
          placeholder={searchPlaceholder()}
          class="w-full px-3 py-1.5 pr-7 text-xs bg-bg-secondary border border-border rounded-lg
            focus:border-accent focus:ring-1 focus:ring-accent/20 outline-none transition-all"
        />
        {#if searchQuery}
          <button onclick={() => { if (searchTimer) clearTimeout(searchTimer); searchTimer = null; searchQuery = ''; searching = false; loadSessions(); }}
            class="absolute right-2 top-1/2 -translate-y-1/2 text-text-muted hover:text-text text-xs leading-none">
            &times;
          </button>
        {/if}
      </div>
      <!-- Search scope: 对话 / 工具 / 思考 -->
      <div class="flex gap-1 shrink-0">
        {#each SCOPES as k}
          <button onclick={() => toggleScope(k)} title={t("sess.scopeBtnTitle", { label: t("sess.kind." + k) })}
            class="text-[10px] px-2 py-1 rounded border transition-colors {scopes.includes(k) ? 'bg-accent text-white border-accent' : 'border-border text-text-muted hover:bg-bg-hover'}">{t("sess.kind." + k)}</button>
        {/each}
      </div>
      <!-- Deep full-text search: scans session source files for precise substrings beyond the
           4000-char index preview (slower, explicit action). -->
      <button
        onclick={runDeepSearch}
        disabled={deepSearching || !searchQuery.trim() || searchInputDisabled()}
        title={t("sess.deepTitle")}
        class="text-[10px] px-2 py-1 rounded border border-border text-text-muted hover:bg-bg-hover transition-colors shrink-0 disabled:opacity-50"
      >
        {deepSearching ? t("sess.deepRunning") : t("sess.deep")}
      </button>
      <!-- Rebuild the search index to pick up new / continued sessions without restarting. -->
      <button
        onclick={rebuildIndex}
        disabled={rebuilding}
        title={t("sess.rebuildTitle")}
        class="text-[10px] px-2 py-1 rounded border border-border text-text-muted hover:bg-bg-hover transition-colors shrink-0 disabled:opacity-50"
      >
        {rebuilding ? t("sess.rebuilding") : t("sess.rebuild")}
      </button>
    {/if}
  </div>

  {#if error}
    <div class="mb-4 p-3 bg-danger-dim border border-danger/30 rounded-xl text-sm text-danger shrink-0">{error}</div>
  {/if}
  {#if notice}
    <div class="mb-4 p-3 bg-accent-dim border border-accent/30 rounded-xl text-xs text-accent shrink-0 break-all">{notice}</div>
  {/if}
  {#if indexStatus && !indexReady}
    <div class="mb-4 flex shrink-0 items-start gap-3 rounded-xl border border-warning/35 bg-warning-dim px-4 py-3" role="status">
      <span class="mt-0.5 h-3.5 w-3.5 shrink-0 animate-spin rounded-full border-2 border-warning/30 border-t-warning"></span>
      <div>
        <div class="text-xs font-medium text-warning">{t("sess.indexBuildNoticeTitle")}</div>
        <div class="mt-1 text-[11px] leading-relaxed text-text-secondary">{t("sess.indexBuildNoticeBody")}</div>
      </div>
    </div>
  {/if}

  {#if detail}
    <div class="flex items-center justify-between mb-3 shrink-0 gap-2">
      <button onclick={closeDetail} class="text-xs text-accent hover:underline shrink-0">{t("sess.back")}</button>
      <div class="flex-1 flex items-center gap-1 min-w-0">
        <div class="flex-1 relative min-w-0">
          <input
            bind:value={detailSearch}
            oninput={onDetailSearchInput}
            placeholder={searchingInSession ? t("sess.searching") : t("sess.inSessionSearch")}
            class="w-full px-2.5 py-1 pr-6 text-xs bg-bg-secondary border border-border rounded-lg focus:border-accent outline-none"
          />
          {#if detailSearch}
            <button onclick={() => { detailSearchRequestSeq++; searchingInSession = false; detailSearch = ''; searchHits = []; currentHitIdx = -1; }}
              class="absolute right-1.5 top-1/2 -translate-y-1/2 text-text-muted hover:text-text text-xs leading-none">
              &times;
            </button>
          {/if}
        </div>
        {#if searchHits.length > 0}
          <button onclick={() => jumpToHit(currentHitIdx)} title={t("sess.jumpToHit")}
            disabled={loadingDetail || searchingInSession}
            class="text-[10px] text-text-muted shrink-0 hover:text-accent cursor-pointer disabled:opacity-30 disabled:cursor-not-allowed">{currentHitIdx + 1}/{searchHits.length}</button>
          <button onclick={() => jumpToHit(Math.max(0, currentHitIdx - 1))}
            disabled={loadingDetail || searchingInSession || currentHitIdx <= 0}
            class="text-[10px] px-1 py-0.5 border border-border rounded hover:bg-bg-hover disabled:opacity-30">&#9650;</button>
          <button onclick={() => jumpToHit(Math.min(searchHits.length - 1, currentHitIdx + 1))}
            disabled={loadingDetail || searchingInSession || currentHitIdx >= searchHits.length - 1}
            class="text-[10px] px-1 py-0.5 border border-border rounded hover:bg-bg-hover disabled:opacity-30">&#9660;</button>
        {/if}
      </div>
      <div class="flex items-center gap-1.5 shrink-0">
        <div class="flex rounded-lg border border-border overflow-hidden">
          <button onclick={() => changeLevel("content")} disabled={loadingDetail}
            class="text-[10px] px-2 py-1 transition-colors disabled:opacity-50 {displayLevel === 'content' ? 'bg-accent text-white' : 'hover:bg-bg-hover'}">{t("sess.levelContent")}</button>
          <button onclick={() => changeLevel("tool")} disabled={loadingDetail}
            class="text-[10px] px-2 py-1 border-l border-border transition-colors disabled:opacity-50 {displayLevel === 'tool' ? 'bg-accent text-white' : 'hover:bg-bg-hover'}">{t("sess.levelTool")}</button>
          <button onclick={() => changeLevel("debug")} disabled={loadingDetail}
            class="text-[10px] px-2 py-1 border-l border-border transition-colors disabled:opacity-50 {displayLevel === 'debug' ? 'bg-accent text-white' : 'hover:bg-bg-hover'}">DEBUG</button>
        </div>
        <button onclick={jumpToLatest} disabled={loadingDetail}
          class="text-[10px] px-2 py-1 border border-border rounded-lg hover:bg-bg-hover disabled:opacity-50">
          {loadingDetail ? '...' : t("sess.latest")}
        </button>
      </div>
    </div>

    <div class="bg-bg-secondary border border-border rounded-xl p-4 mb-3 shrink-0 flex items-center justify-between">
      <div class="min-w-0">
        <div class="flex items-center gap-2 min-w-0">
          <div class="text-sm font-medium truncate">{sessionTitle(detail.session, detail.session.first_prompt)}</div>
          {#if hasModelContexts(detail.session)}
            <span
              title={modelContextsTitle(detail.session)}
              aria-label="Model contexts"
              class="shrink-0 inline-flex h-4 w-4 items-center justify-center rounded-full border border-border text-[9px] font-medium text-text-muted"
            >i</span>
          {/if}
        </div>
        <div class="text-[10px] text-text-secondary mt-1 truncate" title={detail.session.project_path}>
          {#if hostLabel(detail.session)}
            <span class="mr-1.5 rounded bg-bg-tertiary px-1.5 py-0.5 text-[9px] text-text-muted">{hostLabel(detail.session)}</span>
          {/if}
          {displayPath(detail.session.project_path)}
        </div>
        <div class="text-xs text-text-secondary mt-1 flex flex-wrap items-center gap-x-3 gap-y-1">
          <button
            onclick={() => copySessionId(detail!.session)}
            title={t("sess.copyIdTitle", { id: detail.session.session_id })}
            class="rounded bg-bg-tertiary px-1.5 py-0.5 font-mono text-[10px] text-text-muted hover:text-accent"
          >ID {shortSessionId(detail.session)}</button>
          {#if createdTime(detail.session)}<span>{t("sess.createdAt")} {createdTime(detail.session)}</span>{/if}
          {#if updatedTime(detail.session)}<span>{t("sess.updatedAt")} {updatedTime(detail.session)}</span>{/if}
          <span>{formatSize(detail.session.file_size_bytes)}</span>
          {#if detail.subagents.length > 0} · {t("sess.subagentsN", { n: detail.subagents.length })}{/if}
          {#if detail.hasEarlier} · <span class="text-text-muted">{t("sess.tailLoaded")}</span>{/if}
        </div>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <button
          onclick={() => detail && toggleFavorite(detail.session)}
          title={metaFor(detail.session).favorite ? t("sess.unfavorite") : t("sess.favorite")}
          class="px-2 py-1.5 text-[13px] leading-none border border-border rounded-lg hover:bg-bg-hover transition-colors {metaFor(detail.session).favorite ? 'text-warning' : 'text-text-muted'}"
        >{metaFor(detail.session).favorite ? "★" : "☆"}</button>
        <button
          onclick={() => detail && togglePinned(detail.session)}
          title={metaFor(detail.session).pinned ? t("sess.unpin") : t("sess.pin")}
          class="px-2 py-1.5 text-[11px] leading-none border border-border rounded-lg hover:bg-bg-hover transition-colors {metaFor(detail.session).pinned ? 'text-accent' : 'text-text-muted'}"
        >▲</button>
        <button
          onclick={() => (metaPanelOpen ? (metaPanelOpen = false) : openMetaPanel())}
          title={t("sess.noteTags")}
          class="px-3 py-1.5 text-[11px] border border-border rounded-lg hover:bg-bg-hover transition-colors {metaPanelOpen ? 'bg-bg-tertiary' : ''}"
        >{t("sess.note")}{metaFor(detail.session).tags.length > 0 || metaFor(detail.session).note ? " •" : ""}</button>
        <button
          onclick={copyDetail}
          disabled={exporting}
          title={t("sess.copyTitle")}
          class="px-3 py-1.5 text-[11px] border border-border rounded-lg hover:bg-bg-hover transition-colors disabled:opacity-50"
        >
          {exporting ? t("sess.processing") : t("sess.copy")}
        </button>
        <div class="relative" bind:this={exportMenuEl}>
          <button
            onclick={() => (exportMenuOpen = !exportMenuOpen)}
            disabled={exporting}
            title={t("sess.exportTitle")}
            class="px-3 py-1.5 text-[11px] border border-border rounded-lg hover:bg-bg-hover transition-colors disabled:opacity-50"
          >
            {exporting ? t("sess.processing") : t("sess.export")}
          </button>
          {#if exportMenuOpen}
            <div class="absolute right-0 top-full z-20 mt-1 w-28 rounded-lg border border-border bg-bg-secondary py-1 shadow-lg">
              <button onclick={() => exportAs("md")} class="block w-full px-3 py-1.5 text-left text-[11px] hover:bg-bg-hover">Markdown</button>
              <button onclick={() => exportAs("json")} class="block w-full px-3 py-1.5 text-left text-[11px] hover:bg-bg-hover">JSON</button>
              <button onclick={() => exportAs("html")} class="block w-full px-3 py-1.5 text-left text-[11px] hover:bg-bg-hover">HTML</button>
            </div>
          {/if}
        </div>
        <button
          onclick={toggleProjectContext}
          class="px-3 py-1.5 text-[11px] border border-border rounded-lg hover:bg-bg-hover transition-colors {contextOpen ? 'bg-bg-tertiary' : ''}"
        >
          {t("sess.projectContext")}{projectContext ? ` ${projectContextCount()}` : ""}
        </button>
        {#if detail.session.archive_name}
          <span class="px-2 py-1 text-[10px] bg-warning-dim text-warning rounded-lg">{t("sess.archivedLabel", { name: detail.session.archive_name })}</span>
        {:else if canResumeSession(detail.session)}
          <button
            onclick={() => detail && api.shell.resumeSession(detail.session.project_path, detail.session.session_id, detail.session.source)}
            class="px-3 py-1.5 text-[11px] font-medium bg-accent text-white rounded-lg hover:bg-accent-hover transition-all"
          >
            {t("sess.resume")}
          </button>
        {:else}
          <span class="px-2 py-1 text-[10px] bg-bg-tertiary text-text-muted rounded-lg">
            {t("sess.sessionOf", { src: sourceLabel(detail.session) })}
          </span>
        {/if}
      </div>
    </div>

    {#if metaPanelOpen}
      <div class="bg-bg-secondary border border-border rounded-xl p-4 mb-3 shrink-0">
        <div class="flex items-center justify-between gap-3 mb-2">
          <h3 class="text-sm font-medium">{t("sess.noteTags")}</h3>
          <span class="text-[10px] text-text-muted">{t("sess.noteLocalOnly")}</span>
        </div>
        <input
          bind:value={metaTagsDraft}
          placeholder={t("sess.tagsPlaceholder")}
          class="w-full px-3 py-1.5 text-xs bg-bg border border-border rounded-lg focus:border-accent outline-none transition-all"
        />
        <textarea
          bind:value={metaNoteDraft}
          placeholder={t("sess.notePlaceholder")}
          class="mt-2 h-28 w-full resize-none rounded-lg border border-border bg-bg p-3 text-xs outline-none focus:border-accent"
        ></textarea>
        <div class="mt-2 flex justify-end gap-2">
          <button onclick={() => (metaPanelOpen = false)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("common.cancel")}</button>
          <button onclick={saveMetaPanel} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover">{t("sess.save")}</button>
        </div>
      </div>
    {/if}

    {#if contextOpen}
      <div class="bg-bg-secondary border border-border rounded-xl p-4 mb-3 shrink-0">
        <div class="flex items-center justify-between gap-3 mb-3">
          <div>
            <h3 class="text-sm font-medium">{t("ctx.title")}</h3>
            <p class="mt-0.5 text-[10px] text-text-muted">{sourceLabel(detail.session)} · {detail.session.project_path}</p>
          </div>
          {#if loadingContext}
            <span class="text-[10px] text-text-muted">{t("common.loading")}</span>
          {:else if projectContext}
            <span class="text-[10px] text-text-muted">{t("ctx.foundManaged", { n: projectContextCount(), m: projectContextExpectedCount() })}</span>
          {/if}
        </div>

        {#if contextError}
          <div class="mb-3 rounded-lg border border-danger/30 bg-danger-dim p-2 text-xs text-danger">{contextError}</div>
        {/if}

        {#if loadingContext}
          <p class="text-xs text-text-secondary">{t("ctx.reading")}</p>
        {:else if projectContext}
          <div class="grid gap-3 md:grid-cols-2">
            <section>
              <div class="mb-2 flex items-center justify-between gap-2">
                <h4 class="text-[10px] font-medium uppercase tracking-wider text-text-muted">{t("ctx.instructions")}</h4>
                <span class="text-[10px] text-text-muted">{projectContext.instructions.filter((item) => item.exists).length}/{projectContext.instructions.length}</span>
              </div>
              {#if projectContext.instructions.length > 0}
                <div class="space-y-2">
                  {#each projectContext.instructions as item}
                    <details class="rounded-lg border border-border bg-bg p-3">
                      <summary class="flex cursor-pointer items-center gap-2 text-xs font-medium">
                        <span class="min-w-0 flex-1 truncate">{item.title}</span>
                        <span class="shrink-0 rounded bg-bg-tertiary px-1.5 py-0.5 text-[10px] text-text-muted">{artifactStatusLabel(item)}</span>
                      </summary>
                      <div class="mt-2 flex items-center gap-2 text-[10px] text-text-muted">
                        <span class="min-w-0 flex-1 truncate font-mono">{item.path}</span>
                        {#if item.editable && contextEditingArtifactKey !== artifactKey(item)}
                          <button
                            onclick={(event) => { event.preventDefault(); startContextArtifactEdit(item); }}
                            class="shrink-0 rounded border border-border px-2 py-1 hover:bg-bg-hover"
                          >{artifactActionLabel(item)}</button>
                        {/if}
                      </div>
                      {#if contextEditingArtifactKey === artifactKey(item)}
                        <textarea
                          bind:value={contextArtifactContent}
                          class="mt-3 h-64 w-full resize-none rounded-lg border border-border bg-bg-secondary p-3 font-mono text-xs outline-none focus:border-accent"
                          spellcheck="false"
                        ></textarea>
                        <div class="mt-2 flex justify-end gap-2">
                          <button onclick={() => (contextEditingArtifactKey = null)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("common.cancel")}</button>
                          <button onclick={() => saveContextArtifact(item)} disabled={contextSaving} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50">
                            {contextSaving ? t("common.saving") : t("common.save")}
                          </button>
                        </div>
                      {:else}
                        <div class="mt-3 max-h-80 overflow-y-auto">
                          {#if item.exists}
                            <Markdown content={item.content || t("common.emptyFile")} />
                          {:else}
                            <p class="text-xs text-text-secondary">{t("ctx.notCreatedFile")}</p>
                          {/if}
                        </div>
                      {/if}
                    </details>
                  {/each}
                </div>
              {:else}
                <div class="rounded-lg border border-border bg-bg p-3 text-xs text-text-secondary">
                  {projectContext.instructions_status.message}
                </div>
              {/if}
            </section>

            <section>
              <div class="mb-2 flex items-center justify-between gap-2">
                <h4 class="text-[10px] font-medium uppercase tracking-wider text-text-muted">{t("ctx.rules")}</h4>
                <span class="text-[10px] text-text-muted">{projectContext.rules.length}</span>
              </div>
              {#if projectContext.rules.length > 0}
                <div class="space-y-2">
                  {#each projectContext.rules as rule}
                    <details class="rounded-lg border border-border bg-bg p-3">
                      <summary class="cursor-pointer truncate text-xs font-medium">{rule.filename}</summary>
                      <div class="mt-2 flex items-center gap-2 text-[10px] text-text-muted">
                        <span class="min-w-0 flex-1 truncate font-mono">{rule.path}</span>
                        <span class="shrink-0 rounded bg-bg-tertiary px-1.5 py-0.5">{rule.scope === "project" ? t("ctx.scopeProject") : t("ctx.scopeGlobal")}</span>
                        {#if rule.toggleable}
                          <button
                            onclick={(event) => { event.preventDefault(); toggleContextRule(rule); }}
                            disabled={contextSaving}
                            class="shrink-0 rounded border border-border px-2 py-1 hover:bg-bg-hover disabled:opacity-50"
                          >{rule.enabled ? t("ctx.disable") : t("ctx.enable")}</button>
                        {:else}
                          <span class="shrink-0 rounded bg-bg-tertiary px-1.5 py-0.5">{t("common.readonly")}</span>
                        {/if}
                      </div>
                      <div class="mt-3 max-h-80 overflow-y-auto">
                        <Markdown content={rule.content || t("common.emptyFile")} />
                      </div>
                    </details>
                  {/each}
                </div>
              {:else}
                <div class="rounded-lg border border-border bg-bg p-3 text-xs text-text-secondary">
                  {projectContext.rules_status.message}
                </div>
              {/if}
            </section>

            <section>
              <div class="mb-2 flex items-center justify-between gap-2">
                <h4 class="text-[10px] font-medium uppercase tracking-wider text-text-muted">{t("ctx.memories")}</h4>
                <div class="flex items-center gap-2">
                  <span class="text-[10px] text-text-muted">{projectContext.memories.length}</span>
                  {#if projectContext.memory_project && projectContext.memory_status.writable && !contextCreatingMemory}
                    <button onclick={startContextMemoryCreate} class="rounded border border-border px-2 py-1 text-[10px] hover:bg-bg-hover">{t("ctx.new")}</button>
                  {/if}
                </div>
              </div>
              <p class="mb-2 text-[10px] text-text-muted">{projectContext.memory_status.message}</p>
              {#if contextCreatingMemory}
                <div class="mb-2 rounded-lg border border-border bg-bg p-3">
                  <div class="grid grid-cols-2 gap-2 md:grid-cols-4">
                    <input bind:value={contextNewMemoryFilename} placeholder={t("ctx.filenamePlaceholder")} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent" />
                    <input bind:value={contextMemoryName} placeholder={t("common.namePlaceholder")} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent" />
                    <input bind:value={contextMemoryDesc} placeholder={t("common.descPlaceholder")} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent" />
                    <select bind:value={contextMemoryType} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent">
                      <option value="project">project</option>
                      <option value="feedback">feedback</option>
                      <option value="user">user</option>
                      <option value="reference">reference</option>
                    </select>
                  </div>
                  <textarea
                    bind:value={contextMemoryContent}
                    class="mt-3 h-48 w-full resize-none rounded-lg border border-border bg-bg-secondary p-3 font-mono text-xs outline-none focus:border-accent"
                    spellcheck="false"
                  ></textarea>
                  <div class="mt-2 flex justify-end gap-2">
                    <button onclick={() => (contextCreatingMemory = false)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("common.cancel")}</button>
                    <button onclick={saveContextNewMemory} disabled={contextSaving} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50">
                      {contextSaving ? t("common.saving") : t("common.save")}
                    </button>
                  </div>
                </div>
              {/if}
              {#if projectContext.memories.length > 0}
                <div class="space-y-2">
                  {#each projectContext.memories as memory}
                    <details class="rounded-lg border border-border bg-bg p-3" open={contextFocusedMemory === memory.filename || contextEditingMemory?.filename === memory.filename}>
                      <summary class="cursor-pointer truncate text-xs font-medium">{memory.frontmatter?.name ?? memory.filename}</summary>
                      <div class="mt-2 flex items-center gap-2 text-[10px] text-text-muted">
                        <span class="min-w-0 flex-1 truncate font-mono">{memory.filename}</span>
                        {#if canWriteMemory(memory.source) && contextEditingMemory?.filename !== memory.filename}
                          <button
                            onclick={(event) => { event.preventDefault(); startContextMemoryEdit(memory); }}
                            class="shrink-0 rounded border border-border px-2 py-1 hover:bg-bg-hover"
                          >{t("common.edit")}</button>
                        {/if}
                      </div>
                      {#if contextEditingMemory?.filename === memory.filename}
                        <div class="mt-3 grid grid-cols-3 gap-2">
                          <input bind:value={contextMemoryName} placeholder={t("common.namePlaceholder")} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent" />
                          <input bind:value={contextMemoryDesc} placeholder={t("common.descPlaceholder")} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent" />
                          <select bind:value={contextMemoryType} class="rounded-lg border border-border bg-bg-secondary px-2.5 py-1.5 text-xs outline-none focus:border-accent">
                            <option value="feedback">feedback</option>
                            <option value="project">project</option>
                            <option value="user">user</option>
                            <option value="reference">reference</option>
                          </select>
                        </div>
                        <textarea
                          bind:value={contextMemoryContent}
                          class="mt-3 h-64 w-full resize-none rounded-lg border border-border bg-bg-secondary p-3 font-mono text-xs outline-none focus:border-accent"
                          spellcheck="false"
                        ></textarea>
                        <div class="mt-2 flex justify-end gap-2">
                          <button onclick={() => (contextEditingMemory = null)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("common.cancel")}</button>
                          <button onclick={saveContextMemory} disabled={contextSaving} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50">
                            {contextSaving ? t("common.saving") : t("common.save")}
                          </button>
                        </div>
                      {:else}
                        {#if memory.frontmatter?.description}
                          <p class="mt-2 text-[10px] text-text-muted">{memory.frontmatter.description}</p>
                        {/if}
                        <div class="mt-3 max-h-80 overflow-y-auto">
                          <Markdown content={memory.content || t("common.emptyFile")} onLocalLink={navigateContextMemory} />
                        </div>
                      {/if}
                    </details>
                  {/each}
                </div>
              {:else if !contextCreatingMemory}
                <div class="rounded-lg border border-border bg-bg p-3 text-xs text-text-secondary">
                  {t("ctx.noMemories")}
                </div>
              {/if}
            </section>

            <section>
              <div class="mb-2 flex items-center justify-between gap-2">
                <h4 class="text-[10px] font-medium uppercase tracking-wider text-text-muted">{t("ctx.settings")}</h4>
                <span class="text-[10px] text-text-muted">{projectContext.configs.filter((item) => item.exists).length}/{projectContext.configs.length}</span>
              </div>
              {#if projectContext.configs.length > 0}
                <div class="space-y-2">
                  {#each projectContext.configs as item}
                    <details class="rounded-lg border border-border bg-bg p-3">
                      <summary class="flex cursor-pointer items-center gap-2 text-xs font-medium">
                        <span class="min-w-0 flex-1 truncate">{item.title}</span>
                        <span class="shrink-0 rounded bg-bg-tertiary px-1.5 py-0.5 text-[10px] text-text-muted">{artifactStatusLabel(item)}</span>
                      </summary>
                      <div class="mt-2 flex items-center gap-2 text-[10px] text-text-muted">
                        <span class="min-w-0 flex-1 truncate font-mono">{item.path}</span>
                        {#if item.editable && contextEditingArtifactKey !== artifactKey(item)}
                          <button
                            onclick={(event) => { event.preventDefault(); startContextArtifactEdit(item); }}
                            class="shrink-0 rounded border border-border px-2 py-1 hover:bg-bg-hover"
                          >{artifactActionLabel(item)}</button>
                        {/if}
                      </div>
                      {#if contextEditingArtifactKey === artifactKey(item)}
                        <textarea
                          bind:value={contextArtifactContent}
                          class="mt-3 h-64 w-full resize-none rounded-lg border border-border bg-bg-secondary p-3 font-mono text-xs outline-none focus:border-accent"
                          spellcheck="false"
                        ></textarea>
                        <div class="mt-2 flex justify-end gap-2">
                          <button onclick={() => (contextEditingArtifactKey = null)} class="rounded-lg border border-border px-3 py-1.5 text-xs hover:bg-bg-hover">{t("common.cancel")}</button>
                          <button onclick={() => saveContextArtifact(item)} disabled={contextSaving} class="rounded-lg bg-accent px-3 py-1.5 text-xs text-white hover:bg-accent-hover disabled:opacity-50">
                            {contextSaving ? t("common.saving") : t("common.save")}
                          </button>
                        </div>
                      {:else}
                        {#if item.exists}
                          <pre class="mt-3 max-h-80 overflow-y-auto whitespace-pre-wrap rounded-lg bg-bg-tertiary p-3 font-mono text-[10px] text-text-secondary">{item.content || t("common.emptyFile")}</pre>
                        {:else}
                          <p class="mt-3 text-xs text-text-secondary">{t("ctx.notCreatedSetting")}</p>
                        {/if}
                      {/if}
                    </details>
                  {/each}
                </div>
              {:else}
                <div class="rounded-lg border border-border bg-bg p-3 text-xs text-text-secondary">
                  {t("ctx.noSettingsDeclared")}
                </div>
              {/if}
            </section>
          </div>
        {:else}
          <p class="text-xs text-text-secondary">{t("ctx.nothing")}</p>
        {/if}
      </div>
    {/if}

    <!-- svelte-ignore a11y_no_noninteractive_tabindex (the region is the keyboard-scroll viewport) -->
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="flex-1 overflow-y-auto space-y-2 px-2" bind:this={scrollContainer}
      role="region" aria-label={t("sess.title")} tabindex="0"
      onscroll={onTimelineScroll} onwheel={onTimelineUserScroll} ontouchmove={onTimelineUserScroll}
      onkeydown={onTimelineKeyScroll} onpointerdown={onTimelinePointerDown}>
      <div bind:this={topEl} class="h-1"></div>
      {#if loadingDetail && detail.hasEarlier}
        <div class="text-center text-xs text-text-muted py-2">{t("common.loading")}</div>
      {/if}
      <Timeline records={detail.records} highlight={activeHighlight} session={detail.session} subagents={detail.subagents} />

      {#if loadingDetail && detail.hasMore}
        <div class="text-center text-xs text-text-muted py-2">{t("common.loading")}</div>
      {/if}
      <div bind:this={bottomEl} class="h-1"></div>
    </div>

  {:else if loading}
    <p class="text-text-secondary text-sm">{t("sess.loadingSessions")}</p>
  {:else}
    <div class="mb-2 flex shrink-0 flex-wrap items-center gap-1.5">
      <button
        onclick={() => (onlyFavorites = !onlyFavorites)}
        class="text-[11px] px-2 py-1 rounded-lg border transition-colors {onlyFavorites ? 'bg-warning-dim text-warning border-warning/40' : 'border-border text-text-muted hover:bg-bg-hover'}"
      >{t("sess.favFilter")}</button>
      {#each allTags as tag}
        <button
          onclick={() => (activeTag = activeTag === tag ? null : tag)}
          class="text-[11px] px-2 py-1 rounded-lg border transition-colors {activeTag === tag ? 'bg-accent text-white border-accent' : 'border-border text-text-muted hover:bg-bg-hover'}"
        >#{tag}</button>
      {/each}
    </div>
    <div class="mb-2 shrink-0 text-[11px] text-text-muted">
      {#if searching}
        {t("sess.searching")}
      {:else if searchQuery.trim()}
        {t("sess.foundN", { n: visibleSessions.length })}{sessions.length >= MAX_SEARCH_RESULTS ? t("sess.atLimit") : ""}
      {:else}
        {t("sess.totalN", { n: visibleSessions.length })}{onlyFavorites || activeTag ? t("sess.ofN", { n: sessions.length }) : ""}
      {/if}
    </div>
    <div class="flex-1 overflow-y-auto space-y-1" onscroll={onListScroll}>
      {#each renderedSessions as s}
        {@const cacheKey = sessionCacheKey(s)}
        {@const prompt = s.first_prompt ?? promptCache[cacheKey]}
        {@const meta = metaFor(s)}
        <div class="relative bg-bg-secondary border border-border rounded-xl hover:border-border-hover transition-colors">
          <button onclick={() => openSession(s)} class="w-full text-left p-3">
            <div class="flex items-center justify-between">
              <span class="text-xs font-medium truncate pr-14" title={sessionTitle(s, prompt)}>
                {@html highlightPlain(sessionTitle(s, prompt), searchQuery)}
              </span>
            </div>
            {#if shouldShowPromptSubtitle(s, prompt)}
              <div class="mt-1 truncate pr-10 text-[10px] text-text-muted">
                {#if prompt !== undefined && prompt !== null}
                  {@html highlightPlain(prompt || t("sess.noPrompt"), searchQuery)}
                {:else}
                  <span class="italic">{t("common.loading")}</span>
                {/if}
              </div>
            {/if}
            <div class="text-[10px] text-text-secondary mt-1 flex items-center gap-2 flex-wrap">
              {#if allMode}
                <span class="px-1.5 py-0.5 bg-accent-dim text-accent rounded text-[9px]">{sourceLabel(s)}</span>
              {/if}
              {#if s.archive_name}
                <span class="px-1.5 py-0.5 bg-warning-dim text-warning rounded text-[9px]">{t("sess.archivedLabel", { name: s.archive_name })}</span>
              {/if}
              {#if hostLabel(s)}
                <span class="px-1.5 py-0.5 bg-bg-tertiary text-text-muted rounded text-[9px]" title={s.project_path}>{hostLabel(s)}</span>
              {/if}
              <span>{displayPath(s.project_path)}</span>
              <span class="w-1 h-1 rounded-full bg-border"></span>
              <span class="font-mono" title={s.session_id}>ID {shortSessionId(s)}</span>
              <span class="w-1 h-1 rounded-full bg-border"></span>
              {#if createdTime(s)}
                <span title={t("sess.createdAt")}>{t("sess.createdShort")} {createdTime(s)}</span>
                <span class="w-1 h-1 rounded-full bg-border"></span>
              {/if}
              {#if updatedTime(s)}
                <span title={t("sess.updatedAt")}>{t("sess.updatedShort")} {updatedTime(s)}</span>
                <span class="w-1 h-1 rounded-full bg-border"></span>
              {/if}
              <span>{formatSize(s.file_size_bytes)}</span>
              {#if hasModelContexts(s)}
                <span class="w-1 h-1 rounded-full bg-border"></span>
                <span
                  title={modelContextsTitle(s)}
                  aria-label="Model contexts"
                  class="inline-flex h-3.5 w-3.5 items-center justify-center rounded-full border border-border text-[8px] font-medium text-text-muted"
                >i</span>
              {/if}
              {#if s.subagent_count > 0}
                <span class="w-1 h-1 rounded-full bg-border"></span>
                <span>{t("sess.subagentsN", { n: s.subagent_count })}</span>
              {/if}
            </div>
            {#if meta.tags.length > 0}
              <div class="mt-1.5 flex flex-wrap gap-1">
                {#each meta.tags as tag}
                  <span class="px-1.5 py-0.5 bg-bg-tertiary text-text-muted rounded text-[9px]">#{tag}</span>
                {/each}
              </div>
            {/if}
          </button>
          <div class="absolute top-2.5 right-2.5 flex items-center gap-1.5">
            <button onclick={() => togglePinned(s)} title={meta.pinned ? t("sess.unpin") : t("sess.pin")}
              class="text-[11px] leading-none {meta.pinned ? 'text-accent' : 'text-text-muted opacity-40 hover:opacity-100'}">▲</button>
            <button onclick={() => toggleFavorite(s)} title={meta.favorite ? t("sess.unfavorite") : t("sess.favorite")}
              class="text-[13px] leading-none {meta.favorite ? 'text-warning' : 'text-text-muted opacity-40 hover:opacity-100'}">{meta.favorite ? "★" : "☆"}</button>
          </div>
        </div>
      {/each}
      {#if renderLimit < visibleSessions.length}
        <div class="py-3 text-center text-[11px] text-text-muted">{t("sess.showingN", { n: renderLimit, m: visibleSessions.length })}</div>
      {/if}
      {#if visibleSessions.length === 0}<p class="text-sm text-text-secondary">{onlyFavorites || activeTag ? t("sess.noFilterMatch") : emptySessionsLabel()}</p>{/if}
    </div>
  {/if}
</div>
