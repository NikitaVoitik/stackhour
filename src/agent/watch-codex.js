// Codex activity: tail ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl.
// One store covers Codex CLI, the IDE extension, and the Codex desktop app
// (originator field: "codex_cli_rs" vs "Codex Desktop"). Cloud tasks don't
// write local rollouts and are invisible here.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readNewLines, pruneOffsets } from './tail.js';

const CODEX_SESSIONS = path.join(os.homedir(), '.codex', 'sessions');
const RECENT_WINDOW_S = 3600;

function* rolloutFiles(dir, depth = 0) {
  if (depth > 4) return;
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) yield* rolloutFiles(full, depth + 1);
    else if (e.name.startsWith('rollout-') && e.name.endsWith('.jsonl')) yield full;
  }
}

function sourceFromOriginator(originator) {
  const o = String(originator || '').toLowerCase();
  if (o.includes('desktop')) return 'codex-desktop';
  if (o.includes('vscode') || o.includes('ide')) return 'codex-ide';
  return 'codex-cli';
}

export async function watchCodex(cfg, state) {
  if (!fs.existsSync(CODEX_SESSIONS)) return [];
  state.codexOffsets ||= {};
  state.codexMeta ||= {}; // per-file {cwd, source} learned from meta lines
  const offsets = state.codexOffsets;
  const now = Date.now() / 1000;
  const rows = [];
  const files = [];

  for (const file of rolloutFiles(CODEX_SESSIONS)) {
    files.push(file);
    let st;
    try { st = fs.statSync(file); } catch { continue; }
    const firstSight = offsets[file] === undefined;
    if (!firstSight && st.size <= offsets[file]) continue;

    const meta = (state.codexMeta[file] ||= {});
    // on first sight of an actively-written file, read the head once for session_meta
    if (firstSight && st.size > 0) {
      try {
        const head = fs.readFileSync(file, { encoding: 'utf8', flag: 'r' }).slice(0, 65536).split('\n')[0];
        const first = JSON.parse(head);
        const payload = first.payload || first;
        if (first.type === 'session_meta' || payload.cwd) {
          meta.cwd = payload.cwd || meta.cwd;
          meta.source = sourceFromOriginator(payload.originator);
        }
      } catch { /* ignore */ }
    }

    for (const line of readNewLines(file, offsets)) {
      const payload = line.payload || line;
      if (line.type === 'session_meta') {
        meta.cwd = payload.cwd || meta.cwd;
        meta.source = sourceFromOriginator(payload.originator);
        continue;
      }
      if (line.type === 'turn_context' && payload.cwd) { meta.cwd = payload.cwd; continue; }

      const ts = Date.parse(line.timestamp) / 1000;
      if (!Number.isFinite(ts) || now - ts > RECENT_WINDOW_S) continue;
      if (line.type !== 'response_item' && line.type !== 'event_msg') continue;

      const cwd = meta.cwd || 'unknown';
      const base = {
        time: ts,
        source: meta.source || 'codex-cli',
        project: path.basename(cwd),
        category: 'ai coding',
      };
      // file-level entities from patch events when present
      const changes = payload?.changes || payload?.patch?.changes;
      if (changes && typeof changes === 'object' && !Array.isArray(changes)) {
        for (const fp of Object.keys(changes)) {
          rows.push({ ...base, entity: fp, entity_type: 'file', is_write: 1 });
        }
      } else {
        rows.push({ ...base, entity: cwd, entity_type: 'app', is_write: 0 });
      }
    }
  }
  pruneOffsets(offsets, files);
  pruneOffsets(state.codexMeta, files);
  return rows;
}
