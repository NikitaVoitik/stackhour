#!/usr/bin/env node
// Runs on GCP, invoked by the Mac worker over SSH. Atomically claims the oldest pending
// `mac` job, blocking up to ~25s for one to appear; prints its JSON (or nothing) and exits.
// Every poll refreshes the worker-heartbeat so the coordinator knows the Mac is online.
import { readFileSync, writeFileSync, readdirSync, renameSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const JOBS = join(HERE, 'jobs'), PROG = join(HERE, 'inprogress'), HEARTBEAT = join(HERE, 'worker-heartbeat');
for (const d of [JOBS, PROG]) mkdirSync(d, { recursive: true });
const beat = () => { try { writeFileSync(HEARTBEAT, String(Date.now())); } catch {} };

function tryClaim() {
  let files; try { files = readdirSync(JOBS).filter((f) => f.endsWith('.json')).sort(); } catch { return null; }
  for (const f of files) {
    try { renameSync(join(JOBS, f), join(PROG, f)); return readFileSync(join(PROG, f), 'utf8'); } catch {} // lost race — try next
  }
  return null;
}
const deadline = Date.now() + 25000;
(async () => {
  for (;;) {
    beat();
    const j = tryClaim();
    if (j) { process.stdout.write(j); process.exit(0); }
    if (Date.now() > deadline) process.exit(0);
    await new Promise((r) => setTimeout(r, 1000));
  }
})();
