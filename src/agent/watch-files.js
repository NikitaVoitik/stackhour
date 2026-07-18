// Editor-agnostic file activity: scan configured project roots for files
// modified since the last tick. Covers WebStorm, Zed local saves, and Zed
// remote saves (the remote host sees the writes) with zero editor plugins.
import fs from 'node:fs';
import path from 'node:path';
import { resolveProject } from '../project.js';

const LANG_BY_EXT = {
  '.ts': 'TypeScript', '.tsx': 'TypeScript', '.js': 'JavaScript', '.jsx': 'JavaScript',
  '.mjs': 'JavaScript', '.cjs': 'JavaScript', '.json': 'JSON', '.css': 'CSS',
  '.scss': 'SCSS', '.html': 'HTML', '.md': 'Markdown', '.py': 'Python',
  '.rs': 'Rust', '.go': 'Go', '.cs': 'C#', '.sh': 'Shell', '.yml': 'YAML',
  '.yaml': 'YAML', '.toml': 'TOML', '.sql': 'SQL', '.vue': 'Vue', '.svelte': 'Svelte',
};

function* walk(dir, ignoreDirs, depth, maxDepth) {
  if (depth > maxDepth) return;
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    if (e.name.startsWith('.') && e.name !== '.env') continue;
    const full = path.join(dir, e.name);
    if (e.isDirectory()) {
      if (ignoreDirs.includes(e.name)) continue;
      yield* walk(full, ignoreDirs, depth + 1, maxDepth);
    } else if (e.isFile()) {
      yield full;
    }
  }
}

export function gitBranch(projectDir, cache = {}) {
  if (projectDir in cache) return cache[projectDir];
  let branch = null;
  try {
    const dotGit = path.join(projectDir, '.git');
    let gitDir = dotGit;
    if (fs.statSync(dotGit).isFile()) {
      const pointer = fs.readFileSync(dotGit, 'utf8').trim().match(/^gitdir:\s*(.+)$/i);
      if (!pointer) throw new Error('invalid gitdir pointer');
      gitDir = path.resolve(projectDir, pointer[1]);
    }
    const head = fs.readFileSync(path.join(gitDir, 'HEAD'), 'utf8').trim();
    branch = head.startsWith('ref: ') ? head.slice(5).replace('refs/heads/', '') : head.slice(0, 12);
  } catch { /* not a git repo */ }
  cache[projectDir] = branch;
  return branch;
}

export async function watchFiles(cfg, state) {
  const now = Date.now() / 1000;
  const previous = state.filesLastScan || now - cfg.agent.intervalSeconds;
  // Recover if the wall clock moved backwards after the previous tick.
  const since = previous > now ? now - cfg.agent.intervalSeconds : previous;
  state.filesLastScan = now;

  const rows = [];
  const branchCache = {}; // per-tick — branch switches show up next tick
  for (const root of cfg.agent.projectRoots) {
    for (const file of walk(root, cfg.agent.ignoreDirs, 0, cfg.agent.maxScanDepth)) {
      let st;
      try { st = fs.statSync(file); } catch { continue; }
      const mtime = st.mtimeMs / 1000;
      if (mtime <= since || mtime > now + 60) continue;
      // project = first directory level under the root (or the root itself)
      const rel = path.relative(root, file);
      const top = rel.split(path.sep)[0];
      const inSubdir = rel.includes(path.sep);
      const projectDir = inSubdir ? path.join(root, top) : root;
      rows.push({
        time: mtime,
        source: 'editor-files',
        project: resolveProject(projectDir, cfg.agent),
        entity: file,
        entity_type: 'file',
        category: 'coding',
        language: LANG_BY_EXT[path.extname(file).toLowerCase()] || null,
        branch: gitBranch(projectDir, branchCache),
        is_write: 1,
      });
    }
  }
  return rows;
}
