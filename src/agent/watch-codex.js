// Codex activity: tail ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl.
// One store covers Codex CLI, the IDE extension, and the Codex desktop app
// (originator field: "codex_cli_rs" vs "Codex Desktop"). Cloud tasks don't
// write local rollouts and are invisible here.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readFirstJsonLine, readNewLines, pruneOffsets } from './tail.js';
import { costOf } from '../pricing.js';

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

export async function watchCodex(cfg, state, options = {}) {
  const sessionsDir = options.sessionsDir || CODEX_SESSIONS;
  if (!fs.existsSync(sessionsDir)) return [];
  state.codexOffsets ||= {};
  state.codexMeta ||= {}; // per-file {cwd, source} learned from meta lines
  const offsets = state.codexOffsets;
  const now = options.now ?? Date.now() / 1000;
  const rows = [];
  const files = [];

  for (const file of rolloutFiles(sessionsDir)) {
    files.push(file);
    let st;
    try { st = fs.statSync(file); } catch { continue; }
    const firstSight = offsets[file] === undefined;
    if (!firstSight && st.size <= offsets[file]) continue;

    const meta = (state.codexMeta[file] ||= {});
    // Read the head for session metadata. Retry if first sight caught a partial
    // first line; the tail offset still starts at EOF to avoid historical rows.
    if ((firstSight || !meta.cwd || !meta.source) && st.size > 0) {
      try {
        const first = readFirstJsonLine(file);
        if (!first) throw new Error('missing or oversized session metadata');
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
      if (line.type === 'turn_context') {
        if (payload.cwd) meta.cwd = payload.cwd;
        if (payload.model) meta.model = payload.model;
        continue;
      }

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
      // per-turn token usage rides on token_count events
      const tu = payload?.type === 'token_count'
        ? (payload.info?.last_token_usage || payload.info?.total_token_usage_delta)
        : null;
      const tokenFields = tu ? {
        tokens_in: tu.input_tokens || 0,
        tokens_out: (tu.output_tokens || 0) + (tu.reasoning_output_tokens || 0),
        cost: costOf(meta.model || 'gpt-5', {
          input: Math.max(0, (tu.input_tokens || 0) - (tu.cached_input_tokens || 0)),
          cacheRead: tu.cached_input_tokens || 0,
          output: (tu.output_tokens || 0) + (tu.reasoning_output_tokens || 0),
        }, cfg.pricing),
      } : {};
      // a user_message event is Nikita typing a prompt; everything else is the agent
      const isHumanPrompt = line.type === 'event_msg' && payload?.type === 'user_message';
      // file-level entities from patch events when present
      const changes = payload?.changes || payload?.patch?.changes;
      if (changes && typeof changes === 'object' && !Array.isArray(changes)) {
        for (const fp of Object.keys(changes)) {
          rows.push({ ...base, actor: 'agent', entity: fp, entity_type: 'file', is_write: 1 });
        }
      } else {
        rows.push({ ...base, actor: isHumanPrompt ? 'human' : 'agent', entity: cwd, entity_type: 'app', is_write: 0, ...tokenFields });
      }
    }
  }
  pruneOffsets(offsets, files);
  pruneOffsets(state.codexMeta, files);
  return rows;
}
