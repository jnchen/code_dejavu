/**
 * Start an expensive route load only after navigation has settled for one frame-sized delay.
 * Returning the cleanup function lets Svelte cancel the work when the user immediately clicks
 * another menu item, so abandoned pages do not even enqueue their first backend scan.
 */
export function deferRouteLoad(task: () => void | Promise<void>, delayMs = 16): () => void {
  let cancelled = false;
  const timer = setTimeout(() => {
    if (cancelled) return;
    void task();
  }, delayMs);
  return () => {
    cancelled = true;
    clearTimeout(timer);
  };
}
