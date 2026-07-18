#!/usr/bin/env node
// Claude/Codex bridge COORDINATOR (runs on the always-on GCP box).
//
// - Owns the Telegram chat: single long-poll, command handling, rich-message replies.
// - Runs `claude` or `codex` LOCALLY for the `gcp` target.
// - For the `mac` target, queues a job on disk; the Mac worker (outbound SSH only) claims
//   it, runs the selected engine on the Mac, and writes the result back.
//
// Two independent lanes: a sleeping Mac never blocks /gcp, and /mac jobs queue until the
// Mac wakes. Authorized to a single chat id.

import { spawn } from 'node:child_process';
import { readFileSync, writeFileSync, appendFileSync, readdirSync, unlinkSync, mkdirSync, existsSync, statSync, createWriteStream } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { dirname, extname, join } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const CONFIG = JSON.parse(readFileSync(join(HERE, 'config.json'), 'utf8'));
const STATE_PATH = join(HERE, 'state.json');
const LOG_PATH = join(HERE, 'coordinator.log');
const JOBS = join(HERE, 'jobs'), PROG = join(HERE, 'inprogress'), RESULTS = join(HERE, 'results');
for (const d of [JOBS, PROG, RESULTS]) mkdirSync(d, { recursive: true });
const MEDIA = join(HERE, 'media');
mkdirSync(MEDIA, { recursive: true, mode: 0o700 });
const HEARTBEAT = join(HERE, 'worker-heartbeat');

const { token, chatId, targets, elevenLabsApiKey } = CONFIG;
if (!token || !Number.isSafeInteger(chatId) || !targets?.gcp || !targets?.mac) {
  throw new Error('config.json must define token, an integer chatId, and gcp/mac targets.');
}
const API = `https://api.telegram.org/bot${token}`;
const FILE_API = `https://api.telegram.org/file/bot${token}`;
const MAX_MEDIA_BYTES = CONFIG.maxMediaBytes || 512 * 1024 * 1024;

function log(...a) {
  const line = `[${new Date().toISOString()}] ${a.join(' ')}\n`;
  try { appendFileSync(LOG_PATH, line); } catch {}
  try { process.stdout.write(line); } catch {}
}

// ---------- state ----------
function loadState() {
  try {
    const s = JSON.parse(readFileSync(STATE_PATH, 'utf8'));
    s.sessions ||= {};
    s.active ||= CONFIG.defaultTarget;
    s.engine ||= 'claude';
    s.offset ||= 0;
    // Preserve sessions created before engine selection existed.
    for (const target of ['gcp', 'mac']) {
      if (s.sessions[target] && !s.sessions[`${target}:claude`]) s.sessions[`${target}:claude`] = s.sessions[target];
    }
    return s;
  } catch { return { offset: 0, active: CONFIG.defaultTarget, engine: 'claude', sessions: {} }; }
}
let state = loadState();
function saveState() { try { writeFileSync(STATE_PATH, JSON.stringify(state, null, 2)); } catch (e) { log('saveState err', e.message); } }
function sessionKey(target = state.active, engine = state.engine) { return `${target}:${engine}`; }
function getSession(target = state.active, engine = state.engine) { return state.sessions[sessionKey(target, engine)] || null; }
function setSession(target, engine, value) { state.sessions[sessionKey(target, engine)] = value; saveState(); }

// ---------- telegram ----------
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function tg(method, body) {
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      const res = await fetch(`${API}/${method}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
      const data = await res.json();
      if (data.ok) return data.result;
      if (res.status === 429 && data.parameters?.retry_after) { await sleep((data.parameters.retry_after + 1) * 1000); continue; }
      if (/not modified/i.test(data.description || '')) return null;
      if (res.status === 400 || res.status === 404) { log(`tg ${method} 400/404:`, data.description); return null; }
      throw new Error(data.description || `HTTP ${res.status}`);
    } catch (e) { if (attempt === 4) { log(`tg ${method} failed:`, e.message); return null; } await sleep(500 * (attempt + 1)); }
  }
}
async function sendMessage(text, parse_mode, extra = {}) { return tg('sendMessage', { chat_id: chatId, text, disable_web_page_preview: true, ...(parse_mode ? { parse_mode } : {}), ...extra }); }
async function editMessage(message_id, text, parse_mode, extra = {}) { return tg('editMessageText', { chat_id: chatId, message_id, text, disable_web_page_preview: true, ...(parse_mode ? { parse_mode } : {}), ...extra }); }
async function deleteMessage(message_id) { tg('deleteMessage', { chat_id: chatId, message_id }); }
async function typing() { tg('sendChatAction', { chat_id: chatId, action: 'typing' }); }
async function react(message_id, emoji) { tg('setMessageReaction', { chat_id: chatId, message_id, reaction: emoji ? [{ type: 'emoji', emoji }] : [] }); }
async function answerCb(id, text) { tg('answerCallbackQuery', { callback_query_id: id, ...(text ? { text } : {}) }); }
async function sendRich(markdown, extra = {}) {
  const r = await tg('sendRichMessage', { chat_id: chatId, rich_message: { markdown }, ...extra });
  if (r) return r;
  if (Object.keys(extra).length) return tg('sendRichMessage', { chat_id: chatId, rich_message: { markdown } });
  return null;
}

// ---------- inbound media + voice transcription ----------
function safeExt(filePath, fallback) {
  const ext = extname(filePath || '').toLowerCase();
  return /^\.[a-z0-9]{1,10}$/.test(ext) ? ext : fallback;
}
async function downloadTelegramFile(fileId, info) {
  const meta = await tg('getFile', { file_id: fileId });
  if (!meta?.file_path) throw new Error('Telegram did not return a downloadable file path.');
  const size = Number(info.size || meta.file_size || 0);
  if (size > MAX_MEDIA_BYTES) throw new Error(`Attachment is too large (${Math.ceil(size / 1024 / 1024)} MB; limit ${Math.floor(MAX_MEDIA_BYTES / 1024 / 1024)} MB).`);
  const ext = safeExt(meta.file_path, info.kind === 'image' ? '.jpg' : info.kind === 'audio' ? '.ogg' : '.mp4');
  const path = join(MEDIA, `${Date.now()}-${randomUUID()}${ext}`);
  const res = await fetch(`${FILE_API}/${meta.file_path}`);
  if (!res.ok || !res.body) throw new Error(`Telegram download failed (HTTP ${res.status}).`);
  const contentLength = Number(res.headers.get('content-length') || 0);
  if (contentLength > MAX_MEDIA_BYTES) throw new Error(`Attachment exceeds the ${Math.floor(MAX_MEDIA_BYTES / 1024 / 1024)} MB limit.`);
  try { await pipeline(Readable.fromWeb(res.body), createWriteStream(path, { mode: 0o600 })); }
  catch (e) { try { unlinkSync(path); } catch {} throw e; }
  return { path, kind: info.kind, mime: info.mime || 'application/octet-stream', name: info.name || `telegram${ext}`, size: size || contentLength };
}
function attachmentFromMessage(m) {
  if (m.photo?.length) {
    const p = m.photo[m.photo.length - 1];
    return { fileId: p.file_id, kind: 'image', mime: 'image/jpeg', name: 'telegram-photo.jpg', size: p.file_size };
  }
  if (m.video) return { fileId: m.video.file_id, kind: 'video', mime: m.video.mime_type || 'video/mp4', name: m.video.file_name || 'telegram-video.mp4', size: m.video.file_size };
  if (m.video_note) return { fileId: m.video_note.file_id, kind: 'video', mime: 'video/mp4', name: 'telegram-video-note.mp4', size: m.video_note.file_size };
  if (m.animation) return { fileId: m.animation.file_id, kind: 'video', mime: m.animation.mime_type || 'video/mp4', name: m.animation.file_name || 'telegram-animation.mp4', size: m.animation.file_size };
  if (m.document && /^(image|video)\//.test(m.document.mime_type || '')) {
    const kind = m.document.mime_type.startsWith('image/') ? 'image' : 'video';
    return { fileId: m.document.file_id, kind, mime: m.document.mime_type, name: m.document.file_name || `telegram-${kind}`, size: m.document.file_size };
  }
  return null;
}
function voiceFromMessage(m) {
  const a = m.voice || m.audio;
  if (!a) return null;
  return { fileId: a.file_id, kind: 'audio', mime: a.mime_type || (m.voice ? 'audio/ogg' : 'audio/mpeg'), name: a.file_name || (m.voice ? 'telegram-voice.ogg' : 'telegram-audio'), size: a.file_size };
}
function mediaPrompt(text, media, localPath = media.path) {
  const request = (text || '').trim() || (media.kind === 'image' ? 'Please inspect this image and respond.' : 'Please inspect this video and respond.');
  const guidance = media.kind === 'video'
    ? 'Use available tools such as ffmpeg/ffprobe to inspect representative frames and audio when useful.'
    : 'Use the available image inspection tool to view it.';
  return `${request}\n\nTelegram attachment (${media.kind}, ${media.mime}, ${media.name}) is saved locally at: ${localPath}\n${guidance}`;
}
async function transcribeAudio(media) {
  if (!elevenLabsApiKey) throw new Error('ElevenLabs API key is not configured.');
  const form = new FormData();
  form.append('model_id', 'scribe_v2');
  form.append('file', new Blob([readFileSync(media.path)], { type: media.mime }), media.name);
  const res = await fetch('https://api.elevenlabs.io/v1/speech-to-text', { method: 'POST', headers: { 'xi-api-key': elevenLabsApiKey }, body: form });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(`ElevenLabs transcription failed (HTTP ${res.status}${data.detail?.message ? `: ${data.detail.message}` : ''}).`);
  const text = typeof data.text === 'string' ? data.text.trim() : '';
  if (!text) throw new Error('ElevenLabs returned an empty transcript.');
  return text;
}
function pruneMedia() {
  const cutoff = Date.now() - 7 * 24 * 60 * 60 * 1000;
  let files = []; try { files = readdirSync(MEDIA); } catch {}
  for (const f of files) { const p = join(MEDIA, f); try { if (statSync(p).mtimeMs < cutoff) unlinkSync(p); } catch {} }
}

// ---------- keyboards ----------
function controlKb() {
  return { inline_keyboard: [
    [{ text: `${state.engine === 'claude' ? '✅ ' : ''}🧠 Claude`, callback_data: 'e:claude' }, { text: `${state.engine === 'codex' ? '✅ ' : ''}🛠 Codex`, callback_data: 'e:codex' }],
    [{ text: `${state.active === 'mac' ? '✅ ' : ''}🖥️ Mac`, callback_data: 't:mac' }, { text: `${state.active === 'gcp' ? '✅ ' : ''}☁️ GCP`, callback_data: 't:gcp' }],
    [{ text: '🆕 New session', callback_data: 'new' }, { text: 'ℹ️ Status', callback_data: 'where' }],
  ] };
}
const stopKb = () => ({ inline_keyboard: [[{ text: '⏹ Stop', callback_data: 'stop' }]] });
async function registerCommands() {
  await tg('setMyCommands', { commands: [
    { command: 'claude', description: 'Use Claude Code 🧠' }, { command: 'codex', description: 'Use Codex 🛠' },
    { command: 'mac', description: 'Run on the Mac 🖥️' }, { command: 'gcp', description: 'Run on the GCP box ☁️' },
    { command: 'where', description: 'Show active target & session' }, { command: 'new', description: 'Fresh session on active target' },
    { command: 'stop', description: 'Kill/cancel the running job' }, { command: 'menu', description: 'Show tap-button controls' }, { command: 'help', description: 'Show command list' },
  ] });
}

// ---------- rendering + tables ----------
function esc(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); }
function renderHtml(text) {
  const parts = text.split('```'); let out = '';
  for (let i = 0; i < parts.length; i++) {
    if (i % 2 === 1) { let code = parts[i].replace(/^[^\n]*\n/, (m) => (/^[a-zA-Z0-9_+-]*\s*$/.test(m.trim()) ? '' : m)); out += `<pre>${esc(code)}</pre>`; }
    else out += esc(parts[i]).replace(/`([^`\n]+)`/g, (_m, c) => `<code>${c}</code>`);
  }
  return out;
}
function isSep(line) { return line.includes('-') && /^\s*\|?[\s:|-]*-{1,}[\s:|-]*\|?\s*$/.test(line); }
function splitRow(line) { let s = line.trim(); if (s.startsWith('|')) s = s.slice(1); if (s.endsWith('|')) s = s.slice(0, -1); return s.split('|').map((c) => c.trim()); }
function hasTable(t) { const L = t.split('\n'); for (let i = 0; i < L.length - 1; i++) if (L[i].includes('|') && isSep(L[i + 1])) return true; return false; }
function asciiTables(text) {
  const L = text.split('\n'), out = []; let i = 0;
  while (i < L.length) {
    if (L[i].includes('|') && i + 1 < L.length && isSep(L[i + 1])) {
      const header = splitRow(L[i]); const rows = []; let j = i + 2;
      while (j < L.length && L[j].includes('|') && !isSep(L[j])) { rows.push(splitRow(L[j])); j++; }
      const cols = Math.max(header.length, ...rows.map((r) => r.length), 1); const w = Array(cols).fill(0);
      for (const r of [header, ...rows]) for (let c = 0; c < cols; c++) w[c] = Math.max(w[c], (r[c] || '').length);
      const fmt = (r) => Array.from({ length: cols }, (_, c) => (r[c] || '').padEnd(w[c])).join('  ').replace(/\s+$/, '');
      out.push('```\n' + [fmt(header), w.map((x) => '-'.repeat(Math.max(x, 1))).join('  '), ...rows.map(fmt)].join('\n') + '\n```'); i = j;
    } else { out.push(L[i]); i++; }
  }
  return out.join('\n');
}
async function deliverFinal(final, statusId) {
  const sent = await sendRich(final, { reply_markup: controlKb() });
  if (statusId) await deleteMessage(statusId);
  if (!sent) { // fallback: chunked HTML with aligned ASCII tables
    const body = hasTable(final) ? asciiTables(final) : final;
    let s = body; const MAX = 3800;
    while (s.length > MAX) { let cut = s.lastIndexOf('\n', MAX); if (cut < MAX * 0.5) cut = MAX; await sendMessage(renderHtml(s.slice(0, cut)), 'HTML'); s = s.slice(cut); }
    await sendMessage(renderHtml(s) || '…', 'HTML', { reply_markup: controlKb() });
  }
}
function label(name) { return targets[name]?.label || name; }
function engineLabel(name) { return name === 'codex' ? 'Codex' : 'Claude'; }
function fmtDur(ms) { const s = Math.round(ms / 1000); return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m${s % 60}s`; }
function activityLine(tool) {
  if (!tool) return ''; const { name, input } = tool; let d = '';
  if (name === 'Bash') d = input?.command || ''; else if (input?.file_path) d = input.file_path; else if (input?.pattern) d = input.pattern; else if (input?.url) d = input.url; else if (input?.command) d = input.command;
  d = String(d).replace(/\s+/g, ' ').slice(0, 80); return `⚙️ ${name}${d ? ': ' + d : ''}`;
}
function codexActivity(item) {
  if (!item || item.type === 'agent_message') return '';
  let detail = item.command || item.query || item.name || item.path || '';
  detail = String(detail).replace(/\s+/g, ' ').slice(0, 80);
  const names = { command_execution: 'Command', file_change: 'File change', mcp_tool_call: 'Tool', web_search: 'Web search', todo_list: 'Plan' };
  return `⚙️ ${names[item.type] || item.type}${detail ? ': ' + detail : ''}`;
}

// ---------- gcp LOCAL lane ----------
let currentChild = null;
function spawnLocal(engine, tgt, sessionId) {
  if (engine === 'codex') {
    const common = ['--json', '--skip-git-repo-check'];
    if ((tgt.permissionMode || 'default') === 'bypassPermissions') common.push('--dangerously-bypass-approvals-and-sandbox');
    else common.push('--sandbox', 'workspace-write');
    if (tgt.codexModel) common.push('--model', tgt.codexModel);
    const args = sessionId ? ['exec', 'resume', ...common, sessionId, '-'] : ['exec', ...common, '-'];
    return spawn(tgt.codexBin || 'codex', args, { cwd: tgt.cwd, env: { ...process.env, PATH: tgt.extraPath || process.env.PATH }, stdio: ['pipe', 'pipe', 'pipe'] });
  }
  const args = ['-p', '--output-format', 'stream-json', '--verbose', '--include-partial-messages', '--permission-mode', tgt.permissionMode || 'default'];
  if (tgt.model) args.push('--model', tgt.model);
  if (sessionId) args.push('--resume', sessionId);
  return spawn(tgt.claudeBin, args, { cwd: tgt.cwd, env: { ...process.env, PATH: tgt.extraPath || process.env.PATH }, stdio: ['pipe', 'pipe', 'pipe'] });
}
async function runLocal(name, engine, prompt, onStatus) {
  const tgt = targets[name]; let sessionId = getSession(name, engine);
  const attempt = (resume) => new Promise((resolve) => {
    const child = spawnLocal(engine, tgt, resume); currentChild = child;
    let buf = '', text = '', capturedSession = resume, stderr = '', lastStatus = 0;
    child.stdin.write(prompt); child.stdin.end();
    child.stdout.on('data', (d) => {
      buf += d.toString(); let nl;
      while ((nl = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, nl); buf = buf.slice(nl + 1); if (!line.trim()) continue;
        let ev; try { ev = JSON.parse(line); } catch { continue; }
        if (ev.type === 'thread.started' && ev.thread_id) capturedSession = ev.thread_id;
        if (ev.session_id) capturedSession = capturedSession || ev.session_id;
        if (ev.type === 'item.completed' && ev.item?.type === 'agent_message') {
          if (ev.item.text) text = text ? `${text}\n\n${ev.item.text}` : ev.item.text;
        } else if ((ev.type === 'item.started' || ev.type === 'item.completed') && onStatus && Date.now() - lastStatus > 800) {
          const line = codexActivity(ev.item); if (line) { lastStatus = Date.now(); onStatus(line); }
        } else if (ev.type === 'assistant' && Array.isArray(ev.message?.content)) {
          const t = ev.message.content.filter((c) => c.type === 'text').map((c) => c.text).join(''); if (t) text = t;
          const tools = ev.message.content.filter((c) => c.type === 'tool_use');
          if (tools.length && onStatus && Date.now() - lastStatus > 800) { lastStatus = Date.now(); onStatus(activityLine(tools[tools.length - 1])); }
        } else if (ev.type === 'result') { if (ev.session_id) capturedSession = ev.session_id; if (typeof ev.result === 'string' && ev.result.length >= text.length) text = ev.result; }
      }
    });
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    child.on('error', (e) => resolve({ error: e.message, text, sessionId: capturedSession }));
    child.on('close', (code) => { currentChild = null; resolve({ code, text, sessionId: capturedSession, stderr }); });
  });
  let r = await attempt(sessionId);
  if (r.code && r.code !== 0 && sessionId && !r.text) { log(`${engine} resume failed on ${name} (${r.code}); retrying fresh`); r = await attempt(null); }
  if (r.sessionId) setSession(name, engine, r.sessionId);
  return r;
}
let busyLocal = false; const localQueue = [];
async function drainLocal() {
  if (busyLocal) return; busyLocal = true;
  while (localQueue.length) {
    const { prompt, engine, media } = localQueue.shift(); const name = 'gcp'; const t0 = Date.now();
    let statusId = null, lastShown = '';
    const showStatus = async (line) => { const txt = `▹ ${engineLabel(engine)} · ${label(name)} · ${line || 'working…'}`; if (txt === lastShown) return; lastShown = txt; if (statusId) await editMessage(statusId, esc(txt), 'HTML', { reply_markup: stopKb() }); else { const m = await sendMessage(txt, undefined, { reply_markup: stopKb() }); if (m) statusId = m.message_id; } };
    await showStatus('working…'); typing();
    try {
      const r = await runLocal(name, engine, media ? mediaPrompt(prompt, media) : prompt, (line) => { showStatus(line); typing(); });
      let final = r.text || (r.error ? `⚠️ Error: ${r.error}` : r.code ? `⚠️ ${engineLabel(engine)} exited (code ${r.code}).\n${(r.stderr || '').slice(-500)}` : '(no output)');
      final += `\n\n— ${engineLabel(engine)} · ${label(name)} · ${fmtDur(Date.now() - t0)}`;
      await deliverFinal(final, statusId);
    } catch (e) { log('drainLocal err', e.message); if (statusId) await editMessage(statusId, esc(`⚠️ ${e.message}`), 'HTML', { reply_markup: controlKb() }); }
  }
  busyLocal = false;
}

// ---------- mac WORKER lane ----------
const pending = new Map(); // id -> { statusId, t0 }
function workerAlive() { try { return Date.now() - Number(readFileSync(HEARTBEAT, 'utf8')) < 60000; } catch { return false; } }
async function dispatchMac(prompt, engine, media = null) {
  const id = randomUUID(); const t0 = Date.now();
  writeFileSync(join(JOBS, id + '.json'), JSON.stringify({ id, prompt, engine, media, sessionId: getSession('mac', engine), ts: t0 }));
  const alive = workerAlive();
  const txt = alive ? `▹ ${engineLabel(engine)} · ${label('mac')} · working…` : `▹ ${engineLabel(engine)} · ${label('mac')} · queued (Mac offline — runs when it wakes)`;
  const m = await sendMessage(txt, undefined, { reply_markup: stopKb() });
  pending.set(id, { statusId: m?.message_id || null, t0, engine });
}
function pollResults() {
  let files; try { files = readdirSync(RESULTS).filter((f) => f.endsWith('.json')); } catch { return; }
  for (const f of files) {
    const p = join(RESULTS, f); let res; try { res = JSON.parse(readFileSync(p, 'utf8')); } catch { continue; }
    try { unlinkSync(p); } catch {}
    const id = f.replace(/\.json$/, ''); const info = pending.get(id); pending.delete(id);
    const engine = res.engine || info?.engine || 'claude';
    if (res.sessionId) setSession('mac', engine, res.sessionId);
    let final = res.text || (res.error ? `⚠️ Mac error: ${res.error}` : res.code ? `⚠️ ${engineLabel(engine)} exited on Mac (code ${res.code}).` : '(no output)');
    final += `\n\n— ${engineLabel(engine)} · ${label('mac')} · ${info ? fmtDur(Date.now() - info.t0) : 'done'}`;
    deliverFinal(final, info?.statusId || null);
  }
}
function cancelQueuedMac() {
  let n = 0, running = 0;
  for (const [id, info] of pending) {
    const jp = join(JOBS, id + '.json');
    if (existsSync(jp)) { try { unlinkSync(jp); } catch {} if (info.statusId) editMessage(info.statusId, '🛑 Cancelled.', undefined, {}); pending.delete(id); n++; }
    else running++; // already claimed by the worker — can't interrupt remotely
  }
  return { cancelled: n, running };
}

// ---------- commands ----------
const HELP = ['<b>Claude + Codex bridge</b> (distributed)', '', '🧠 /claude — use Claude Code', '🛠 /codex — use Codex', '🖥️ /mac — run on the Mac', '☁️ /gcp — run on the GCP box', 'ℹ️ /where — active engine, target &amp; session', '🆕 /new — fresh session for this engine + target', '⏹ /stop — kill/cancel the running job', '🎛 /menu — tap-button controls', '', '<i>Anything else → selected engine on the active target.</i>'].join('\n');
function switchTarget(name) { state.active = name; saveState(); return `Switched to ${label(name)} with ${engineLabel(state.engine)}. ${getSession() ? '(resuming session)' : '(new session)'}`; }
function switchEngine(name) { state.engine = name; saveState(); return `Switched to ${engineLabel(name)} on ${label(state.active)}. ${getSession() ? '(resuming session)' : '(new session)'}`; }
function statusText() { const session = getSession(); return `Engine: ${engineLabel(state.engine)}\nTarget: ${label(state.active)}\nSession: ${session ? session.slice(0, 8) + '…' : 'none (fresh)'}\nMac worker: ${workerAlive() ? 'online' : 'offline'}\nGCP busy: ${busyLocal ? 'yes' : 'no'}`; }

async function handleText(text, msgId) {
  const lower = text.toLowerCase();
  if (lower === '/start' || lower === '/help') return void sendMessage(HELP, 'HTML', { reply_markup: controlKb() });
  if (lower === '/menu') return void sendMessage(`🎛 Controls — ${engineLabel(state.engine)} on ${label(state.active)}`, undefined, { reply_markup: controlKb() });
  if (lower === '/claude') return void sendMessage(switchEngine('claude'), undefined, { reply_markup: controlKb() });
  if (lower === '/codex') return void sendMessage(switchEngine('codex'), undefined, { reply_markup: controlKb() });
  if (lower === '/mac' || lower === '/local') return void sendMessage(switchTarget('mac'), undefined, { reply_markup: controlKb() });
  if (lower === '/gcp' || lower === '/remote') return void sendMessage(switchTarget('gcp'), undefined, { reply_markup: controlKb() });
  if (lower === '/where' || lower === '/status') return void sendMessage(statusText(), undefined, { reply_markup: controlKb() });
  if (lower === '/new' || lower === '/reset') { setSession(state.active, state.engine, null); return void sendMessage(`🆕 Fresh ${engineLabel(state.engine)} session on ${label(state.active)}.`); }
  if (lower === '/stop') {
    let msg = [];
    if (currentChild) { try { currentChild.kill('SIGTERM'); } catch {} msg.push('🛑 Stopped GCP job.'); }
    const { cancelled, running } = cancelQueuedMac();
    if (cancelled) msg.push(`🛑 Cancelled ${cancelled} queued Mac job(s).`);
    if (running) msg.push(`⚠️ ${running} Mac job(s) already running — can't interrupt remotely yet.`);
    return void sendMessage(msg.length ? msg.join('\n') : 'Nothing running.');
  }
  if (lower.startsWith('/')) return void sendMessage('Unknown command.\n\n' + HELP, 'HTML', { reply_markup: controlKb() });
  routePrompt(text, msgId);
}
function routePrompt(text, msgId, media = null) {
  if (msgId) react(msgId, '👀');
  if (state.active === 'mac') dispatchMac(text, state.engine, media);
  else { localQueue.push({ prompt: text, engine: state.engine, media }); drainLocal(); }
}
async function handleMediaMessage(m, attachment) {
  react(m.message_id, '👀');
  try {
    const media = await downloadTelegramFile(attachment.fileId, attachment);
    log(`media: ${media.kind} ${media.size || 0} bytes`);
    routePrompt(m.caption || '', null, media);
  } catch (e) { log('media err', e.message); await sendMessage(`⚠️ Could not process attachment: ${e.message}`); }
}
async function handleVoiceMessage(m, voice) {
  react(m.message_id, '👀');
  const status = await sendMessage('🎙️ Transcribing voice message…');
  let media;
  try {
    media = await downloadTelegramFile(voice.fileId, voice);
    const transcript = await transcribeAudio(media);
    const preview = transcript.length > 3400 ? transcript.slice(0, 3400) + '…' : transcript;
    if (status?.message_id) await editMessage(status.message_id, `🎙️ Transcript:\n${preview}`);
    else await sendMessage(`🎙️ Transcript:\n${preview}`);
    log(`voice: transcribed ${media.size || 0} bytes to ${transcript.length} chars`);
    routePrompt(transcript, null);
  } catch (e) {
    log('voice err', e.message);
    const text = `⚠️ Could not transcribe voice message: ${e.message}`;
    if (status?.message_id) await editMessage(status.message_id, text); else await sendMessage(text);
  } finally { if (media?.path) try { unlinkSync(media.path); } catch {} }
}
async function handleCallback(cb) {
  const data = cb.data || '';
  if (data === 'e:claude') { switchEngine('claude'); answerCb(cb.id, 'Using Claude Code'); }
  else if (data === 'e:codex') { switchEngine('codex'); answerCb(cb.id, 'Using Codex'); }
  else if (data === 't:mac') { switchTarget('mac'); answerCb(cb.id, 'On the Mac 🖥️'); }
  else if (data === 't:gcp') { switchTarget('gcp'); answerCb(cb.id, 'On the GCP box ☁️'); }
  else if (data === 'new') { setSession(state.active, state.engine, null); answerCb(cb.id, 'Fresh session'); }
  else if (data === 'where') { answerCb(cb.id, statusText()); }
  else if (data === 'stop') { handleText('/stop'); answerCb(cb.id, 'Stopping…'); }
  else { answerCb(cb.id); return; }
  if (cb.message?.message_id && /🖥️|☁️|🧠|🛠/.test(cb.message.text || '')) editMessage(cb.message.message_id, cb.message.text, undefined, { reply_markup: controlKb() });
}

// ---------- main ----------
async function poll() {
  log(`coordinator online — ${engineLabel(state.engine)} on ${label(state.active)}`);
  pruneMedia();
  setInterval(pruneMedia, 24 * 60 * 60 * 1000);
  await registerCommands();
  setInterval(pollResults, 1000); // deliver Mac worker results
  await sendMessage(`🤖 Claude + Codex bridge online. Active: ${engineLabel(state.engine)} on ${label(state.active)}. Mac worker: ${workerAlive() ? 'online' : 'offline'}.`, undefined, { reply_markup: controlKb() });
  for (;;) {
    const updates = await tg('getUpdates', { offset: state.offset, timeout: 50, allowed_updates: ['message', 'edited_message', 'callback_query'] });
    if (!updates) { await sleep(1000); continue; }
    for (const u of updates) {
      state.offset = u.update_id + 1; saveState();
      if (u.callback_query) { if (u.callback_query.message?.chat?.id === chatId) handleCallback(u.callback_query); continue; }
      const m = u.message || u.edited_message;
      if (!m) continue;
      if (m.chat?.id !== chatId) { log(`ignoring chat ${m.chat?.id}`); continue; }
      if (m.from?.is_bot) continue;
      const voice = voiceFromMessage(m);
      const attachment = attachmentFromMessage(m);
      if (voice) { handleVoiceMessage(m, voice); continue; }
      if (attachment) { handleMediaMessage(m, attachment); continue; }
      if (!m.text) continue;
      log(`msg: ${m.text.slice(0, 80)}`);
      handleText(m.text, m.message_id);
    }
  }
}
process.on('uncaughtException', (e) => log('uncaught', e.stack || e.message));
process.on('unhandledRejection', (e) => log('unhandled', e?.stack || String(e)));
poll();
