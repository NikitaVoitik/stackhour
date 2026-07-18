// Linux only: human presence over SSH. A pty's atime updates when the shell
// reads input (i.e. on keystrokes) — the same signal `w` uses for idle. For
// each recently-active pty we find the foreground process and use its cwd to
// attribute the time to a project. This makes "typing over SSH" count as
// human time even when no file was saved and no agent was involved.
import fs from 'node:fs';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { resolveProject } from '../project.js';

const exec = promisify(execFile);

async function foregroundCwd(pts) {
  // pick the foreground process ('+' in stat) on this tty, else the newest
  try {
    const { stdout } = await exec('ps', ['-t', pts, '-o', 'pid=,stat=']);
    const procs = stdout.trim().split('\n').map((l) => {
      const [pid, stat] = l.trim().split(/\s+/);
      return { pid, fg: (stat || '').includes('+') };
    }).filter((p) => p.pid);
    if (!procs.length) return null;
    const pick = procs.find((p) => p.fg) || procs[procs.length - 1];
    return fs.readlinkSync(`/proc/${pick.pid}/cwd`);
  } catch { return null; }
}

export async function watchSsh(cfg, state) {
  const now = Date.now() / 1000;
  const rows = [];
  let entries;
  try { entries = fs.readdirSync('/dev/pts'); } catch { return rows; }
  for (const name of entries) {
    if (!/^\d+$/.test(name)) continue;
    let st;
    try { st = fs.statSync(`/dev/pts/${name}`); } catch { continue; }
    const idle = now - st.atimeMs / 1000;
    if (idle >= cfg.agent.idleSeconds) continue;
    const cwd = await foregroundCwd(`pts/${name}`) || 'unknown';
    rows.push({
      time: now,
      source: 'ssh',
      project: resolveProject(cwd, cfg.agent),
      entity: cwd,
      entity_type: 'app',
      category: 'coding',
      actor: 'human',
      is_write: 0,
    });
  }
  return rows;
}
