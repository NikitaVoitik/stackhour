#!/usr/bin/env node
// Runs on GCP, invoked by the Mac worker over SSH with the job result piped on stdin.
// Writes results/<id>.json (the coordinator's watcher picks it up) and clears in-progress.
import { writeFileSync, unlinkSync, mkdirSync, renameSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const RESULTS = join(HERE, 'results'), PROG = join(HERE, 'inprogress');
mkdirSync(RESULTS, { recursive: true });
const id = process.argv[2];
if (!id) { console.error('return.mjs: missing job id'); process.exit(2); }
if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(id)) {
  console.error('return.mjs: invalid job id'); process.exit(2);
}

let data = ''; process.stdin.setEncoding('utf8');
process.stdin.on('data', (d) => (data += d));
process.stdin.on('end', () => {
  const tmp = join(RESULTS, id + '.json.tmp');
  writeFileSync(tmp, data || '{}');
  // atomic publish so the coordinator never reads a half-written file
  try { unlinkSync(join(RESULTS, id + '.json')); } catch {}
  try { renameSync(tmp, join(RESULTS, id + '.json')); } catch (e) { writeFileSync(join(RESULTS, id + '.json'), data || '{}'); }
  try { unlinkSync(join(PROG, id + '.json')); } catch {}
  process.exit(0);
});
