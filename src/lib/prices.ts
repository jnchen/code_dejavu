import type { PriceRow } from "$lib/types";

// Usage cost is an estimate. The pricing table lives in the app config (DejavuConfig.prices),
// edited in Settings and persisted by the Rust backend. These defaults mirror the backend's
// `default_prices()` and are used by the Settings "reset to defaults" action.
export const DEFAULT_PRICES: PriceRow[] = [
  { match: "opus", input: 15, output: 75 },
  { match: "sonnet", input: 3, output: 15 },
  { match: "haiku", input: 0.8, output: 4 },
  { match: "o3", input: 15, output: 60 },
  { match: "o1", input: 15, output: 60 },
  { match: "gpt-5", input: 1.25, output: 10 },
  { match: "gpt-4o", input: 2.5, output: 10 },
  { match: "gpt-4", input: 2.5, output: 10 },
  { match: "gemini", input: 1.25, output: 5 },
];

/** Find the price for a model by case-insensitive substring match against the given rows.
 *  Returns null when no row matches (caller treats it as "unpriced"). */
export function priceForIn(
  rows: PriceRow[] | null | undefined,
  model: string
): { input: number; output: number } | null {
  if (!rows) return null;
  const m = model.toLowerCase();
  const hit = rows.find((p) => p.match && m.includes(p.match.toLowerCase()));
  return hit ? { input: hit.input, output: hit.output } : null;
}
