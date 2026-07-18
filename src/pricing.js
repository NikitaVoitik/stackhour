// API-equivalent cost estimates per model, USD per million tokens.
// (If you're on a Claude Max / ChatGPT subscription the real marginal cost is
// $0 — treat these as "what this agent work would have cost via API".)
// Override any of this via the "pricing" key in config.json; the longest
// key that is a substring of the model id wins.
export const DEFAULT_PRICING = {
  'claude-fable': { in: 10, out: 50, cacheRead: 1, cacheWrite: 12.5 },
  'claude-opus': { in: 5, out: 25, cacheRead: 0.5, cacheWrite: 6.25 },
  'claude-sonnet': { in: 3, out: 15, cacheRead: 0.3, cacheWrite: 3.75 },
  'claude-haiku': { in: 1, out: 5, cacheRead: 0.1, cacheWrite: 1.25 },
  'gpt-5': { in: 1.25, out: 10, cacheRead: 0.125, cacheWrite: 0 },
  default: { in: 3, out: 15, cacheRead: 0.3, cacheWrite: 3.75 },
};

export function priceFor(model, pricing = DEFAULT_PRICING) {
  const id = String(model || '').toLowerCase();
  let best = pricing.default || DEFAULT_PRICING.default;
  let bestLen = 0;
  for (const [key, p] of Object.entries(pricing)) {
    if (key !== 'default' && id.includes(key) && key.length > bestLen) {
      best = p;
      bestLen = key.length;
    }
  }
  return best;
}

// usage: {input, cacheRead, cacheWrite, output} token counts
export function costOf(model, usage, pricing) {
  const p = priceFor(model, pricing);
  return (
    (usage.input || 0) * p.in
    + (usage.cacheRead || 0) * p.cacheRead
    + (usage.cacheWrite || 0) * p.cacheWrite
    + (usage.output || 0) * p.out
  ) / 1e6;
}
