// Turn raw heartbeats into time totals.
//
// Credit model (WakaTime-ish, simplified so it's easy to tune):
// sort a stream of heartbeats by time; each one earns min(gap-to-next, capSeconds);
// the last one in a run earns lastEventCreditSeconds.
//
// Stream splitting encodes who can parallelize:
// - human streams split per (machine, source): your attention is single-
//   threaded, so rapid switching between projects in one tool never
//   double-counts — it shows up as switches, not overlap.
// - agent streams additionally split per project: three Claude sessions
//   grinding on three projects at once each accrue real agent-hours.

function groupStreams(rows) {
  const streams = new Map();
  for (const r of rows) {
    const key = JSON.stringify([r.machine, r.source, r.actor,
      r.actor === 'agent' ? r.project : '']);
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
  const totals = new Map();
  for (const r of creditedRows) {
    const key = JSON.stringify(keys.map((k) => r[k] ?? 'unknown'));
    let t = totals.get(key);
    if (!t) totals.set(key, (t = { seconds: 0, tokens: 0, cost: 0 }));
    t.seconds += r.credit;
    t.tokens += (r.tokens_in || 0) + (r.tokens_out || 0);
    t.cost += r.cost || 0;
  }
  return [...totals.entries()]
    .map(([key, t]) => {
      const parts = JSON.parse(key);
      const obj = { seconds: Math.round(t.seconds), tokens: t.tokens, cost: Math.round(t.cost * 100) / 100 };
      keys.forEach((k, i) => (obj[k] = parts[i]));
      return obj;
    })
    .sort((a, b) => b.seconds - a.seconds);
}

export function buildSegments(creditedRows, { joinGapSeconds = 300 } = {}) {
  // contiguous work segments per (project, actor) for the timeline lanes —
  // this is what makes project switches and parallel agent work visible
  const groups = new Map();
  for (const r of creditedRows) {
    const key = JSON.stringify([r.project, r.actor]);
    let g = groups.get(key);
    if (!g) groups.set(key, (g = []));
    g.push(r);
  }
  const segments = [];
  for (const [key, rows] of groups) {
    const [project, actor] = JSON.parse(key);
    rows.sort((a, b) => a.time - b.time);
    let seg = null;
    for (const r of rows) {
      if (seg && r.time - seg.end <= joinGapSeconds) {
        seg.end = r.time;
        seg.seconds += r.credit;
        seg.sources.add(r.source);
      } else {
        if (seg) segments.push({ ...seg, sources: [...seg.sources] });
        seg = { project, actor, start: r.time, end: r.time, seconds: r.credit, sources: new Set([r.source]) };
      }
    }
    if (seg) segments.push({ ...seg, sources: [...seg.sources] });
  }
  return segments.sort((a, b) => a.start - b.start)
    .map((s) => ({ ...s, seconds: Math.round(s.seconds) }));
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
