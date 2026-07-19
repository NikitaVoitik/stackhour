#!/usr/bin/env node
// A LOCAL mock of the Telegram Bot API, used to diff the Node coordinator
// against the Rust bridge without ever touching the real bot token.
//
// SAFETY: this listens on 127.0.0.1 only and is the ONLY endpoint either
// implementation is allowed to talk to during a parity run. Nothing here
// knows the real token; the driver always passes an obviously fake one.
//
// Usage: node mock-bot-api.mjs <out.json> <updates.json>
//   <out.json>     — every recorded request, written on exit (SIGTERM/SIGINT).
//   <updates.json> — the updates handed out by the FIRST getUpdates call.
//                    Every later getUpdates returns [].
//
// Prints `PORT <n>` on stdout once listening.

import { createServer } from 'node:http';
import { readFileSync, writeFileSync } from 'node:fs';

const [outPath, updatesPath] = process.argv.slice(2);
const updates = updatesPath ? JSON.parse(readFileSync(updatesPath, 'utf8')) : [];

const recorded = [];
let servedUpdates = false;
let messageId = 1000;

function result(method, body) {
  switch (method) {
    case 'getUpdates': {
      if (servedUpdates) return [];
      servedUpdates = true;
      return updates;
    }
    // The nonstandard rich method: both implementations must fall back to
    // plain sendMessage, so the mock refuses it the same way for both.
    case 'sendRichMessage':
      return null;
    // The real API rejects anything but an array here. Modelled so a
    // double-wrapped payload fails loudly instead of being recorded as if it
    // had worked.
    case 'setMyCommands':
      return Array.isArray(body.commands) ? true : null;
    case 'sendMessage':
    case 'editMessageText':
      return { message_id: ++messageId, chat: { id: body.chat_id }, text: body.text };
    default:
      return true;
  }
}

const server = createServer((req, res) => {
  let raw = '';
  req.on('data', (c) => (raw += c));
  req.on('end', () => {
    const method = req.url.split('/').pop();
    let body = {};
    try { body = raw ? JSON.parse(raw) : {}; } catch { body = { _unparsed: raw }; }
    recorded.push({ method, body });
    const r = result(method, body);
    if (r === null) {
      res.writeHead(400, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ok: false, error_code: 400, description: 'Bad Request: method not found' }));
      return;
    }
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ ok: true, result: r }));
  });
});

function dump() {
  writeFileSync(outPath, JSON.stringify(recorded, null, 2));
  process.exit(0);
}
process.on('SIGTERM', dump);
process.on('SIGINT', dump);

server.listen(0, '127.0.0.1', () => {
  process.stdout.write(`PORT ${server.address().port}\n`);
});
