// Shared, cached dashboard state. The homepage reads aggregates from the in-memory backend index
// (one cheap `dashboard_summary` call) instead of re-scanning every session file on each visit.
//
// Two caching layers eliminate the "reopen reloads everything" feeling:
//   1. Module-level $state — survives route navigation, so returning to the dashboard is instant.
//   2. localStorage snapshot — survives app restart, rendered immediately, then revalidated
//      in the background (stale-while-revalidate).
import { api } from "$lib/api";
import type { DashboardSummary, IndexStatus, SourceInfo } from "$lib/types";

const LS_KEY = "dejavu:dashboard:v1";

interface Snapshot {
  summary: DashboardSummary | null;
  sources: SourceInfo[];
  savedAt: number;
}

let summary = $state<DashboardSummary | null>(null);
let sources = $state<SourceInfo[]>([]);
let indexStatus = $state<IndexStatus | null>(null);
let loading = $state(false);
let error = $state("");
let lastLoaded = $state(0);

let hydrated = false;
let inflight: Promise<void> | null = null;
let indexPoller: ReturnType<typeof setInterval> | null = null;

function indexReady(status: IndexStatus | null): boolean {
  return !!status && status.Ready != null;
}

/** Load the last persisted snapshot once, so the first paint after an app restart shows data. */
function hydrate() {
  if (hydrated || typeof localStorage === "undefined") return;
  hydrated = true;
  try {
    const raw = localStorage.getItem(LS_KEY);
    if (!raw) return;
    const snap = JSON.parse(raw) as Snapshot;
    if (snap.summary) summary = snap.summary;
    if (snap.sources?.length) sources = snap.sources;
    lastLoaded = snap.savedAt ?? 0;
  } catch {}
}

function persist() {
  if (typeof localStorage === "undefined") return;
  try {
    const snap: Snapshot = { summary, sources, savedAt: Date.now() };
    localStorage.setItem(LS_KEY, JSON.stringify(snap));
  } catch {}
}

async function fetchSummary() {
  const [srcs, sum] = await Promise.all([
    api.sessions.listSources(),
    api.sessions.dashboardSummary(),
  ]);
  sources = srcs;
  summary = sum;
  lastLoaded = Date.now();
  persist();
}

function stopIndexPolling() {
  if (indexPoller) {
    clearInterval(indexPoller);
    indexPoller = null;
  }
}

// While the backend index is still building, its aggregates are empty/partial. Poll until it's
// ready, then pull the final numbers once — no manual refresh needed.
function startIndexPolling() {
  if (indexPoller) return;
  indexPoller = setInterval(async () => {
    try {
      const status = await api.sessions.getIndexStatus();
      indexStatus = status;
      if (indexReady(status)) {
        stopIndexPolling();
        await fetchSummary();
      }
    } catch {}
  }, 800);
}

/** Revalidate in the background. Safe to call repeatedly; concurrent calls share one request. */
function refresh(): Promise<void> {
  if (inflight) return inflight;
  loading = true;
  error = "";
  inflight = (async () => {
    try {
      await fetchSummary();
      const status = await api.sessions.getIndexStatus().catch(() => null);
      indexStatus = status;
      if (!indexReady(status)) startIndexPolling();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
      inflight = null;
    }
  })();
  return inflight;
}

async function rebuildIndex() {
  indexStatus = { Building: null };
  try {
    indexStatus = await api.sessions.rebuildIndex();
    await fetchSummary();
  } catch (e) {
    error = String(e);
  }
}

export const dashboardStore = {
  get summary() {
    return summary;
  },
  get sources() {
    return sources;
  },
  get indexStatus() {
    return indexStatus;
  },
  get indexBuilding() {
    return indexStatus != null && !indexReady(indexStatus);
  },
  get loading() {
    return loading;
  },
  get error() {
    return error;
  },
  get lastLoaded() {
    return lastLoaded;
  },
  hydrate,
  refresh,
  rebuildIndex,
};
