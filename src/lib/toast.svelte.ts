// Minimal global toast queue for surfacing non-fatal failures that would otherwise be swallowed by
// `.catch(() => {})`. Background/best-effort retries and localStorage-availability guards should NOT
// use this (they aren't user-actionable); reserve it for user-initiated actions that silently fail
// (opening a link, copying, loading session marks, …) so the user gets a light, dismissible hint.

export type ToastKind = "error" | "info";
export interface Toast {
  id: number;
  message: string;
  kind: ToastKind;
}

let items = $state<Toast[]>([]);
let seq = 0;

/** Reactive accessor — read inside markup (e.g. `{#each toasts() as t}`) to track updates. */
export function toasts(): Toast[] {
  return items;
}

export function dismissToast(id: number) {
  items = items.filter((t) => t.id !== id);
}

/** Queue a toast. Auto-dismisses after `ttl` ms (0 keeps it until dismissed). */
export function pushToast(message: string, kind: ToastKind = "error", ttl = 4500) {
  const id = ++seq;
  items = [...items, { id, message, kind }];
  if (ttl > 0) {
    setTimeout(() => dismissToast(id), ttl);
  }
}
