// Shared helper: incremental JSONL tailing with per-file byte offsets kept in
// the agent state. On first sight of a file the offset starts at EOF (no
// historical flood); subsequent ticks read only appended bytes.
import fs from 'node:fs';

const MAX_READ_PER_FILE = 5 * 1024 * 1024;
const MAX_HEAD_LINE = 1024 * 1024;

export function readFirstJsonLine(file, maxBytes = MAX_HEAD_LINE) {
  let st;
  try { st = fs.statSync(file); } catch { return null; }
  if (!st.size) return null;
  const len = Math.min(st.size, maxBytes);
  const buf = Buffer.alloc(len);
  let bytesRead = 0;
  let fd;
  try {
    fd = fs.openSync(file, 'r');
    bytesRead = fs.readSync(fd, buf, 0, len, 0);
  } catch { return null; }
  finally { if (fd !== undefined) fs.closeSync(fd); }
  const newline = buf.indexOf(0x0a, 0);
  if (newline < 0 && st.size > bytesRead) return null;
  const end = newline < 0 ? bytesRead : newline;
  try { return JSON.parse(buf.subarray(0, end).toString('utf8')); }
  catch { return null; }
}

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

  // Only commit the offset through the final newline. Writers can briefly
  // expose a partial JSON object at EOF; advancing to st.size here would lose
  // that record forever when the rest arrives on the next tick.
  const lastNewline = buf.lastIndexOf(0x0a);
  if (lastNewline < 0) {
    offsets[file] = start;
    return [];
  }
  offsets[file] = start + lastNewline + 1;

  const lines = [];
  for (const line of buf.subarray(0, lastNewline).toString('utf8').split('\n')) {
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
