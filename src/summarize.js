// Turn raw heartbeats into time totals.
//
// Credit model (WakaTime-ish, simplified so it's easy to tune):
// sort a stream of heartbeats by time; each one earns min(gap-to-next, capSeconds);
// the last one in a run earns lastEventCreditSeconds. Streams are split per
// (machine, source) so two tools active at once both earn time — that's a
// deliberate choice (change groupStreams below for strict wall-clock).

function groupStreams(rows) {
  const streams = new Map();
  for (const r of rows) {
    const key = JSON.stringify([r.machine, r.source]);
    let s = streams.get(key);
    if (!s) streams.set(key, (s = []));
    s.push(r);
  }
  return streams;
}

export function computeCredits(rows, { capSeconds = 120, lastEventCreditSeconds = 60 } = {}) {
  // returns rows annotated with .credit (seconds earned by that heartbeat)
  const out = [];
  for (const stream of groupStreams(rows).values()) {
    stream.sort((a, b) => a.time - b.time);
    for (let i = 0; i < stream.length; i++) {
      const gap = i + 1 < stream.length ? stream[i + 1].time - stream[i].time : Infinity;
      const credit = gap === Infinity ? lastEventCreditSeconds : Math.min(gap, capSeconds);
      out.push({ ...stream[i], credit });
    }
  }
  return out;
}

function aggregate(creditedRows, keyFn) {
  const totals = new Map();
  for (const r of creditedRows) {
    const key = JSON.stringify(keyFn(r));
    totals.set(key, (totals.get(key) || 0) + r.credit);
  }
  return totals;
}

export function totalsBy(creditedRows, keys) {
  // keys: array of field names, e.g. ['project'] or ['project','source']
  const totals = aggregate(creditedRows, (r) => keys.map((k) => r[k] ?? 'unknown'));
  return [...totals.entries()]
    .map(([key, seconds]) => {
      const parts = JSON.parse(key);
      const obj = { seconds: Math.round(seconds) };
      keys.forEach((k, i) => (obj[k] = parts[i]));
      return obj;
    })
    .sort((a, b) => b.seconds - a.seconds);
}

export function dayBuckets(creditedRows, keys, tzOffsetMinutes = 0) {
  // per-local-day totals grouped by keys; tzOffsetMinutes as returned by
  // Date.getTimezoneOffset() (positive = behind UTC)
  const totals = aggregate(creditedRows, (r) => {
    const day = new Date((r.time - tzOffsetMinutes * 60) * 1000).toISOString().slice(0, 10);
    return [day, ...keys.map((k) => r[k] ?? 'unknown')];
  });
  return [...totals.entries()]
    .map(([key, seconds]) => {
      const parts = JSON.parse(key);
      const obj = { date: parts[0], seconds: Math.round(seconds) };
      keys.forEach((k, i) => (obj[k] = parts[i + 1]));
      return obj;
    })
    .sort((a, b) => (a.date < b.date ? -1 : 1));
}
