// Claude Code activity: tail ~/.claude/projects/**/*.jsonl transcripts.
// Covers the CLI, SDK sessions, and Claude Desktop-hosted (Cowork) sessions —
// the entrypoint field distinguishes them.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readNewLines, pruneOffsets } from './tail.js';

const CLAUDE_PROJECTS = path.join(os.homedir(), '.claude', 'projects');
const RECENT_WINDOW_S = 3600; // ignore replayed/old lines beyond this age

function* jsonlFiles(dir, depth = 0) {
  if (depth > 4) return;
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) yield* jsonlFiles(full, depth + 1);
    else if (e.name.endsWith('.jsonl')) yield full;
  }
}

export async function watchClaude(cfg, state) {
  if (!fs.existsSync(CLAUDE_PROJECTS)) return [];
  state.claudeOffsets ||= {};
  const offsets = state.claudeOffsets;
  const now = Date.now() / 1000;
  const rows = [];
  const files = [];

  for (const file of jsonlFiles(CLAUDE_PROJECTS)) {
    files.push(file);
    // cheap skip: untouched files
    let st;
    try { st = fs.statSync(file); } catch { continue; }
    if (offsets[file] !== undefined && st.size <= offsets[file]) continue;

    for (const line of readNewLines(file, offsets)) {
      const ts = Date.parse(line.timestamp) / 1000;
      if (!Number.isFinite(ts) || now - ts > RECENT_WINDOW_S) continue;
      const cwd = line.cwd || 'unknown';
      const source = line.entrypoint === 'claude-desktop' ? 'claude-desktop' : 'claude-code';
      const base = {
        time: ts,
        source,
        project: path.basename(cwd),
        category: 'ai coding',
      };
      // prefer concrete file entities from tool_use blocks
      let emitted = false;
      const content = line.message?.content;
      if (Array.isArray(content)) {
        for (const block of content) {
          const fp = block?.type === 'tool_use' && block.input?.file_path;
          if (fp) {
            rows.push({ ...base, entity: fp, entity_type: 'file', is_write: /edit|write/i.test(block.name || '') ? 1 : 0 });
            emitted = true;
          }
        }
      }
      if (!emitted && (line.type === 'user' || line.type === 'assistant')) {
        rows.push({ ...base, entity: cwd, entity_type: 'app', is_write: 0 });
      }
    }
  }
  pruneOffsets(offsets, files);
  return rows;
}
