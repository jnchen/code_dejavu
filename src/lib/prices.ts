import type { PriceRow } from "$lib/types";

// Usage cost is an estimate. The pricing table lives in the app config (DejavuConfig.prices),
// edited in Settings and persisted by the Rust backend. These defaults mirror the backend's
// `default_prices()` and are used by the Settings "reset to defaults" action.
export const DEFAULT_PRICES: PriceRow[] = [
  { match: "claude-opus", input: 15, output: 75 },
  { match: "claude-sonnet", input: 3, output: 15 },
  { match: "claude-haiku", input: 0.8, output: 4 },
  { match: "claude-fable-5", input: 10, output: 50 },
  { match: "claude-haiku-4-5", input: 1, output: 5 },
  { match: "claude-opus-4-6", input: 5, output: 25 },
  { match: "claude-opus-4-7", input: 5, output: 25 },
  { match: "claude-opus-4-8", input: 5, output: 25 },
  { match: "claude-sonnet-4-6", input: 3, output: 15 },
  { match: "gpt-5", input: 1.25, output: 10 },
  { match: "gpt-5.4", input: 2.5, output: 15 },
  { match: "gpt-5.4-mini", input: 0.75, output: 4.5 },
  { match: "gpt-5.4-pro", input: 30, output: 180 },
  { match: "gpt-5.5", input: 5, output: 30 },
  { match: "gpt-5.6-sol", input: 5, output: 30 },
  { match: "gpt-4o", input: 2.5, output: 10 },
  { match: "gpt-4", input: 2.5, output: 10 },
  { match: "gemini", input: 1.25, output: 5 },
];

function normalizedId(value: string): { full: string; provider: string | null; model: string } {
  const full = value.trim().toLowerCase().replaceAll(".", "-");
  const separator = full.lastIndexOf("/");
  return separator < 0
    ? { full, provider: null, model: full }
    : { full, provider: full.slice(0, separator), model: full.slice(separator + 1) };
}

/** Match a row against both provider-qualified and bare model ids using a strict model prefix. If
 * both sides name a provider, they must name the same one; this prevents cross-provider matches. */
export function priceRowMatchesModel(row: PriceRow, model: string): boolean {
  if (!row.match.trim()) return false;
  const candidate = normalizedId(model);
  const matcher = normalizedId(row.match);
  if (candidate.provider && matcher.provider) {
    return candidate.provider === matcher.provider && candidate.model.startsWith(matcher.model);
  }
  if (matcher.provider) return candidate.model.startsWith(matcher.model);
  return candidate.model.startsWith(matcher.model) || candidate.full.startsWith(matcher.full);
}

/** Find the most specific matching price for a model. Longer prefixes win so a refreshed exact
 * model price can coexist with broad fallback rules such as `gpt-5`. */
export function priceForIn(
  rows: PriceRow[] | null | undefined,
  model: string
): { input: number; output: number } | null {
  if (!rows) return null;
  const hit = rows
    .filter((row) => priceRowMatchesModel(row, model))
    .sort((a, b) => b.match.length - a.match.length)[0];
  return hit ? { input: hit.input, output: hit.output } : null;
}
