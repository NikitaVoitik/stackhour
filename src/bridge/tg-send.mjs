#!/usr/bin/env node
// tg-send — send a message to the owner's Telegram chat from any Claude session.
//
// Usage:
//   node tg-send.mjs "message text"          # message as argument
//   echo "message" | node tg-send.mjs        # message from stdin
//   node tg-send.mjs --html "<b>hi</b>"      # send with HTML parse mode
//   node tg-send.mjs --from "GCP" "done"     # prefix with a source label
//
// Reads token + chatId from the adjacent config.json (or $CLAUDE_REMOTE_CONFIG).
// Exit code 0 on success, non-zero on failure. Prints nothing on success unless --verbose.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);

// A bare `tg-send --help` must never reach Telegram: without this guard the
// flag falls through to `rest` and gets delivered as a live message. Handle
// help before reading config or touching the network.
if (args.includes('-h') || args.includes('--help')) {
  console.log(`tg-send — send a message to the owner's Telegram chat.

Usage:
  node tg-send.mjs "message text"          message as argument
  echo "message" | node tg-send.mjs        message from stdin
  node tg-send.mjs --html "<b>hi</b>"      send with HTML parse mode
  node tg-send.mjs --from "GCP" "done"     prefix with a source label

Reads token + chatId from the adjacent config.json (or $CLAUDE_REMOTE_CONFIG).
Exit code 0 on success, 1 when any part failed, 2 for config/usage problems.
Prints nothing on success unless --verbose.`);
  process.exit(0);
}

let html = false, verbose = false, from = null;
const rest = [];
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  if (a === '--html') html = true;
  else if (a === '--verbose' || a === '-v') verbose = true;
  else if (a === '--from') from = args[++i];
  else rest.push(a);
}

const HERE = dirname(fileURLToPath(import.meta.url));
const cfgPath = process.env.CLAUDE_REMOTE_CONFIG || join(HERE, 'config.json');
let cfg;
try {
  cfg = JSON.parse(readFileSync(cfgPath, 'utf8'));
} catch (e) {
  console.error(`tg-send: cannot read config at ${cfgPath}: ${e.message}`);
  process.exit(2);
}
const { token, chatId } = cfg;
if (!token || !chatId) {
  console.error('tg-send: config missing token or chatId');
  process.exit(2);
}

async function readStdin() {
  const chunks = [];
  for await (const c of process.stdin) chunks.push(c);
  return Buffer.concat(chunks).toString('utf8');
}

let text = rest.join(' ').trim();
if (!text && !process.stdin.isTTY) text = (await readStdin()).trim();
if (!text) {
  console.error('tg-send: no message text provided (argument or stdin)');
  process.exit(2);
}
if (from) text = `[${from}] ${text}`;

// Telegram hard-caps a message at 4096 chars; split on newlines when possible.
function chunk(s, max = 4000) {
  const out = [];
  while (s.length > max) {
    let cut = s.lastIndexOf('\n', max);
    if (cut < max * 0.5) cut = max;
    out.push(s.slice(0, cut));
    s = s.slice(cut);
  }
  out.push(s);
  return out;
}

const API = `https://api.telegram.org/bot${token}`;

// Try to deliver as a native Rich Message (Bot API 10.1) so markdown tables/headings/lists
// render natively. Returns true on success, false if the API rejects it (older API).
async function sendRich(markdown) {
  const res = await fetch(`${API}/sendRichMessage`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ chat_id: chatId, rich_message: { markdown } }),
  });
  const data = await res.json().catch(() => ({}));
  if (data.ok) return true;
  if (res.status === 429 && data.parameters?.retry_after) {
    await new Promise((r) => setTimeout(r, (data.parameters.retry_after + 1) * 1000));
    return sendRich(markdown);
  }
  return false; // fall back to plain
}

async function send(part) {
  for (let attempt = 0; attempt < 4; attempt++) {
    const body = { chat_id: chatId, text: part, disable_web_page_preview: true };
    if (html) body.parse_mode = 'HTML';
    const res = await fetch(`${API}/sendMessage`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    const data = await res.json().catch(() => ({}));
    if (data.ok) return true;
    // Handle rate limiting; retry once without HTML if parse fails.
    if (res.status === 429 && data.parameters?.retry_after) {
      await new Promise((r) => setTimeout(r, (data.parameters.retry_after + 1) * 1000));
      continue;
    }
    if (html && /can't parse|parse entities/i.test(data.description || '')) {
      html = false;
      continue;
    }
    console.error(`tg-send: Telegram error: ${data.description || res.status}`);
    return false;
  }
  return false;
}

let ok = true;
// Default: always try Rich Message first; fall back to plain (chunked) if unsupported.
// --html forces plain HTML mode (skips rich).
if (!html && (await sendRich(text))) {
  if (verbose) console.error('tg-send: sent (rich)');
} else {
  for (const part of chunk(text)) ok = (await send(part)) && ok;
  if (verbose && ok) console.error('tg-send: sent (plain)');
}
process.exit(ok ? 0 : 1);
