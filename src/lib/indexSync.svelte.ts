import { listen } from "@tauri-apps/api/event";
import { t } from "$lib/i18n.svelte";
import { pushToast } from "$lib/toast.svelte";

/**
 * Mirror of the Rust `IndexEvent` payload (services::search::INDEX_EVENT_NAME).
 * The backend announces every index swap — startup cache restore + reconcile,
 * disk-change auto refresh, manual rebuilds and archive operations — so
 * background work is never silent again.
 */
export type IndexEventPayload = {
  phase: "building" | "ready";
  origin: "startup" | "auto" | "manual";
  session_count: number;
  token_count: number;
  failed_files: number;
};

type EmitHook = (payload: IndexEventPayload) => void;

class IndexSync {
  /** True between a "building" event and its "ready" completion. */
  building = $state(false);
  /** Session count of the last ready index. */
  sessionCount = $state(0);
  /**
   * Bumps on every "ready" event. Pages that render index-derived data derive
   * on this (skipping the first, pre-mount value) to reload without a manual
   * refresh — the fix for "sessions showed up minutes late with no sign".
   */
  version = $state(0);
  /** True once at least one ready event arrived (mirrors IndexStatus.Ready). */
  ready = $state(false);

  #lastReadyCount: number | null = null;
  #started = false;

  start(): void {
    if (this.#started || typeof window === "undefined") return;
    this.#started = true;
    // E2E hook: tests (and the mocked Tauri IPC, which has no event plugin)
    // push events through the window handle instead of a real emit.
    (window as unknown as { __E2E_EMIT_INDEX__?: EmitHook }).__E2E_EMIT_INDEX__ = (payload) =>
      this.onEvent(payload);
    listen<IndexEventPayload>("dejavu://index", (event) => this.onEvent(event.payload)).catch(
      () => {
        // Without the event plugin the app still works; polling remains the fallback.
      },
    );
  }

  private onEvent(payload: IndexEventPayload): void {
    if (!payload || (payload.phase !== "building" && payload.phase !== "ready")) return;
    if (payload.phase === "building") {
      this.building = true;
      return;
    }
    this.building = false;
    this.ready = true;
    this.sessionCount = payload.session_count;
    this.version++;
    const previous = this.#lastReadyCount;
    this.#lastReadyCount = payload.session_count;
    // Announce growth only: the startup cache restore fires first with the old
    // count, and the reconcile that follows is exactly the "new source appeared"
    // moment the user needs to hear about. Shrinks (archiving) stay quiet.
    if (previous != null && payload.session_count > previous) {
      pushToast(
        t("toast.indexUpdated", {
          added: payload.session_count - previous,
          n: payload.session_count,
        }),
      );
    }
  }
}

export const indexSync = new IndexSync();
