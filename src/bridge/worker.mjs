#!/usr/bin/env node
// Claude/Codex bridge WORKER (runs on the Mac). Outbound-only: repeatedly SSHes into the GCP
// coordinator to claim `mac` jobs, runs the selected engine locally, and pushes results
// back. Needs no inbound connectivity, no sudo, no Remote Login — just the existing SSH key.

import { spawn } from 'node:child_process';
import { readFileSync, appendFileSync, mkdirSync, readdirSync, statSync, unlinkSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const CFG_PATH = process.env.STACKHOUR_BRIDGE_WORKER_CONFIG || join(HERE, 'worker-config.json');
const CFG = JSON.parse(readFileSync(CFG_PATH, 'utf8'));
if (!CFG.gcpKey || !CFG.gcpSsh || !CFG.remoteDir || !CFG.claudeBin || !CFG.cwd) {
  throw new Error('worker-config.json must define gcpKey, gcpSsh, remoteDir, claudeBin, and cwd.');
}
const LOG = join(HERE, 'worker.log');
const SSH_BASE = ['-i', CFG.gcpKey, '-o', 'BatchMode=yes', '-o', 'ServerAliveInterval=15', '-o', 'ConnectTimeout=15', CFG.gcpSsh];
const REMOTE = CFG.remoteDir;
const NODE = CFG.remoteNode || 'node';
const MEDIA = join(HERE, 'media');
mkdirSync(MEDIA, { recursive: true, mode: 0o700 });

function log(...a) { const l = `[${new Date().toISOString()}] ${a.join(' ')}\n`; try { appendFileSync(LOG, l); } catch {} try { process.stdout.write(l); } catch {} }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Run an ssh command; optionally pipe `stdin`. Resolves {code, stdout, stderr}.
function ssh(remoteCmd, stdin) {
  return new Promise((resolve) => {
    const child = spawn('ssh', [...SSH_BASE, remoteCmd], { stdio: ['pipe', 'pipe', 'pipe'] });
    let out = '', err = '';
    child.stdout.on('data', (d) => (out += d)); child.stderr.on('data', (d) => (err += d));
    child.on('error', (e) => resolve({ code: -1, stdout: '', stderr: e.message }));
    child.on('close', (code) => resolve({ code, stdout: out, stderr: err }));
    if (stdin !== undefined) child.stdin.end(stdin); else child.stdin.end();
  });
}

function downloadMedia(media) {
  return new Promise((resolve) => {
    if (!media?.path || !media.path.startsWith(`${REMOTE}/media/`)) return resolve({ error: 'Invalid remote media path.' });
    const localPath = join(MEDIA, basename(media.path));
    const args = ['-i', CFG.gcpKey, '-o', 'BatchMode=yes', '-o', 'ServerAliveInterval=15', '-o', 'ConnectTimeout=15', `${CFG.gcpSsh}:${media.path}`, localPath];
    const child = spawn('scp', args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let err = '';
    child.stderr.on('data', (d) => (err += d.toString()));
    child.on('error', (e) => resolve({ error: e.message }));
    child.on('close', (code) => resolve(code === 0 ? { path: localPath } : { error: `scp exited ${code}: ${err.trim().slice(-240)}` }));
  });
}
function mediaPrompt(text, media, localPath) {
  const request = (text || '').trim() || (media.kind === 'image' ? 'Please inspect this image and respond.' : 'Please inspect this video and respond.');
  const guidance = media.kind === 'video'
    ? 'Use available tools such as ffmpeg/ffprobe to inspect representative frames and audio when useful.'
    : 'Use the available image inspection tool to view it.';
  return `${request}\n\nTelegram attachment (${media.kind}, ${media.mime}, ${media.name}) is saved locally at: ${localPath}\n${guidance}`;
}
function pruneMedia() {
  const cutoff = Date.now() - 7 * 24 * 60 * 60 * 1000;
  let files = []; try { files = readdirSync(MEDIA); } catch {}
  for (const f of files) { const p = join(MEDIA, f); try { if (statSync(p).mtimeMs < cutoff) unlinkSync(p); } catch {} }
}

// Run claude locally on the Mac for one job; return { text, sessionId, code, error }.
function runClaude(prompt, sessionId) {
  return new Promise((resolve) => {
    const args = ['-p', '--output-format', 'stream-json', '--verbose', '--permission-mode', CFG.permissionMode || 'default'];
    if (CFG.model) args.push('--model', CFG.model);
    if (sessionId) args.push('--resume', sessionId);
    const child = spawn(CFG.claudeBin, args, { cwd: CFG.cwd, env: { ...process.env, PATH: CFG.extraPath || process.env.PATH }, stdio: ['pipe', 'pipe', 'pipe'] });
    let buf = '', text = '', captured = sessionId || null, stderr = '';
    child.stdin.write(prompt); child.stdin.end();
    child.stdout.on('data', (d) => {
      buf += d.toString(); let nl;
      while ((nl = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, nl); buf = buf.slice(nl + 1); if (!line.trim()) continue;
        let ev; try { ev = JSON.parse(line); } catch { continue; }
        if (ev.session_id) captured = captured || ev.session_id;
        if (ev.type === 'assistant' && Array.isArray(ev.message?.content)) { const t = ev.message.content.filter((c) => c.type === 'text').map((c) => c.text).join(''); if (t) text = t; }
        else if (ev.type === 'result') { if (ev.session_id) captured = ev.session_id; if (typeof ev.result === 'string' && ev.result.length >= text.length) text = ev.result; }
      }
    });
    child.stderr.on('data', (d) => (stderr += d.toString()));
    child.on('error', (e) => resolve({ error: e.message, text, sessionId: captured }));
    child.on('close', (code) => resolve({ code, text, sessionId: captured, stderr: stderr.slice(-500) }));
  });
}

// Run Codex locally and capture its JSONL thread id, progress, and final message.
function runCodex(prompt, sessionId) {
  return new Promise((resolve) => {
    const common = ['--json', '--skip-git-repo-check'];
    if ((CFG.permissionMode || 'default') === 'bypassPermissions') common.push('--dangerously-bypass-approvals-and-sandbox');
    else common.push('--sandbox', 'workspace-write');
    if (CFG.codexModel) common.push('--model', CFG.codexModel);
    const args = sessionId ? ['exec', 'resume', ...common, sessionId, '-'] : ['exec', ...common, '-'];
    const child = spawn(CFG.codexBin || 'codex', args, { cwd: CFG.cwd, env: { ...process.env, PATH: CFG.extraPath || process.env.PATH }, stdio: ['pipe', 'pipe', 'pipe'] });
    let buf = '', text = '', captured = sessionId || null, stderr = '';
    child.stdin.write(prompt); child.stdin.end();
    child.stdout.on('data', (d) => {
      buf += d.toString(); let nl;
      while ((nl = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, nl); buf = buf.slice(nl + 1); if (!line.trim()) continue;
        let ev; try { ev = JSON.parse(line); } catch { continue; }
        if (ev.type === 'thread.started' && ev.thread_id) captured = ev.thread_id;
        if (ev.type === 'item.completed' && ev.item?.type === 'agent_message' && ev.item.text) text = text ? `${text}\n\n${ev.item.text}` : ev.item.text;
      }
    });
    child.stderr.on('data', (d) => (stderr += d.toString()));
    child.on('error', (e) => resolve({ error: e.message, text, sessionId: captured }));
    child.on('close', (code) => resolve({ code, text, sessionId: captured, stderr: stderr.slice(-500) }));
  });
}

async function handle(job) {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(job?.id || '')) {
    log('rejected job with invalid id');
    return;
  }
  log(`claimed ${job.id}: ${String(job.prompt).slice(0, 60)}`);
  const engine = job.engine === 'codex' ? 'codex' : 'claude';
  let r;
  let prompt = job.prompt;
  if (job.media) {
    const downloaded = await downloadMedia(job.media);
    if (downloaded.error) r = { error: `Could not download Telegram attachment: ${downloaded.error}`, text: '' };
    else prompt = mediaPrompt(prompt, job.media, downloaded.path);
  }
  try { if (!r) r = await (engine === 'codex' ? runCodex(prompt, job.sessionId) : runClaude(prompt, job.sessionId)); }
  catch (e) { r = { error: e.message, text: '' }; }
  // resume may fail on a stale/rotated session — retry fresh once
  if (r.code && r.code !== 0 && job.sessionId && !r.text) {
    log(`${engine} resume failed (${r.code}); retry fresh`);
    try { r = await (engine === 'codex' ? runCodex(prompt, null) : runClaude(prompt, null)); } catch (e) { r = { error: e.message, text: '' }; }
  }
  const payload = JSON.stringify({ id: job.id, engine, text: r.text || '', sessionId: r.sessionId || null, code: r.code, error: r.error || null });
  const ret = await ssh(`${NODE} ${REMOTE}/return.mjs ${job.id}`, payload);
  if (ret.code !== 0) log(`return failed for ${job.id}: ${ret.stderr.trim()}`);
  else log(`returned ${job.id}`);
}

async function loop() {
  pruneMedia();
  log('worker online');
  for (;;) {
    const res = await ssh(`${NODE} ${REMOTE}/claim.mjs`);
    if (res.code !== 0) { log(`claim ssh failed (${res.code}): ${res.stderr.trim().slice(0, 120)}`); await sleep(5000); continue; }
    const out = res.stdout.trim();
    if (!out) { await sleep(500); continue; } // no job this round (claim blocked ~25s)
    let job; try { job = JSON.parse(out); } catch (e) { log('bad job json:', out.slice(0, 120)); await sleep(1000); continue; }
    await handle(job);
  }
}
process.on('uncaughtException', (e) => log('uncaught', e.stack || e.message));
process.on('unhandledRejection', (e) => log('unhandled', e?.stack || String(e)));
loop();
