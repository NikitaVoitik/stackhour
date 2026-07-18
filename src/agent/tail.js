// Shared helper: incremental JSONL tailing with per-file byte offsets kept in
// the agent state. On first sight of a file the offset starts at EOF (no
// historical flood); subsequent ticks read only appended bytes.
import fs from 'node:fs';

const MAX_READ_PER_FILE = 5 * 1024 * 1024;

export function readNewLines(file, offsets) {
  let st;
  try { st = fs.statSync(file); } catch { return []; }
  const prev = offsets[file];
  if (prev === undefined) { offsets[file] = st.size; return []; }
  if (st.size <= prev) { offsets[file] = Math.min(prev, st.size); return []; }

  const start = st.size - prev > MAX_READ_PER_FILE ? st.size - MAX_READ_PER_FILE : prev;
  const len = st.size - start;
  const buf = Buffer.alloc(len);
  const fd = fs.openSync(file, 'r');
  try { fs.readSync(fd, buf, 0, len, start); } finally { fs.closeSync(fd); }
  offsets[file] = st.size;

  const lines = [];
  for (const line of buf.toString('utf8').split('\n')) {
    const t = line.trim();
    if (!t) continue;
    try { lines.push(JSON.parse(t)); } catch { /* partial/garbled line */ }
  }
  return lines;
}

export function pruneOffsets(offsets, liveFiles, max = 2000) {
  // drop offsets for files that no longer exist once the map grows large
  const keys = Object.keys(offsets);
  if (keys.length < max) return;
  const live = new Set(liveFiles);
  for (const k of keys) if (!live.has(k)) delete offsets[k];
}
