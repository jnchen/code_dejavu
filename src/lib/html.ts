// Escape raw text so it can't inject HTML when used with {@html}.
export function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

// Highlight a query in PLAIN text safely: escape the text first, then wrap matches.
export function highlightPlain(text: string, query: string): string {
  const safe = escapeHtml(text);
  if (!query || query.length < 2) return safe;
  const escaped = query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp(`(${escapeHtml(escaped)}|${escaped})`, "gi");
  return safe.replace(re, '<mark class="bg-warning/40 text-inherit rounded px-0.5">$1</mark>');
}
