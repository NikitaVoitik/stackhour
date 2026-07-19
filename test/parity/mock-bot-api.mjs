#!/usr/bin/env node
// A LOCAL mock of the Telegram Bot API, used to diff the Node coordinator
// against the Rust bridge without ever touching the real bot token.
//
// SAFETY: this listens on 127.0.0.1 only and is the ONLY endpoint either
// implementation is allowed to talk to during a parity run. Nothing here
// knows the real token; the driver always passes an obviously fake one.
//
// Usage: node mock-bot-api.mjs <out.json> <updates.json> [<files.json>]
//   <out.json>     — every recorded request, written on exit (SIGTERM/SIGINT).
//   <updates.json> — the updates handed out by the FIRST getUpdates call.
//                    Every later getUpdates returns [].
//   <files.json>   — optional media fixture:
//                    { files: { <file_id>: { file_path, file_size?, bodyBase64 } },
//                      transcript: "<what speech-to-text returns>",
//                      transcriptStatus: 200 }
//                    It backs three extra endpoints:
//                      POST /bot<token>/getFile
//                      GET  /file/bot<token>/<file_path>
//                      POST /v1/speech-to-text   (the ElevenLabs stand-in)
//
// Prints `PORT <n>` on stdout once listening.

import { createServer } from 'node:http';
import { readFileSync, writeFileSync } from 'node:fs';

const [outPath, updatesPath, filesPath] = process.argv.slice(2);
const updates = updatesPath ? JSON.parse(readFileSync(updatesPath, 'utf8')) : [];
const fixture = filesPath ? JSON.parse(readFileSync(filesPath, 'utf8')) : { files: {} };
const FILES = fixture.files || {};

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
    // Telegram answers getFile with the relative path under the FILE api,
    // not the bot api. An unknown file_id 400s, as the real API does.
    case 'getFile': {
      const f = FILES[body.file_id];
      if (!f) return null;
      return { file_id: body.file_id, file_path: f.file_path, ...(f.file_size ? { file_size: f.file_size } : {}) };
    }
    case 'sendMessage':
    case 'editMessageText':
      return { message_id: ++messageId, chat: { id: body.chat_id }, text: body.text };
    default:
      return true;
  }
}

/** The file-download endpoint: GET /file/bot<token>/<file_path>. */
function serveFileDownload(req, res) {
  const filePath = req.url.replace(/^\/file\/bot[^/]+\//, '');
  const entry = Object.values(FILES).find((f) => f.file_path === filePath);
  recorded.push({ method: 'download', body: { file_path: filePath, found: Boolean(entry) } });
  if (!entry) {
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end('Not Found');
    return;
  }
  const bytes = Buffer.from(entry.bodyBase64 || '', 'base64');
  res.writeHead(200, { 'content-type': 'application/octet-stream', 'content-length': bytes.length });
  res.end(bytes);
}

/** The ElevenLabs stand-in. Never api.elevenlabs.io. */
function serveSpeechToText(req, res, byteLength) {
  recorded.push({
    method: 'speech-to-text',
    // The multipart body is binary; record only its shape, never its content.
    body: { bytes: byteLength, hasKey: Boolean(req.headers['xi-api-key']) },
  });
  const status = fixture.transcriptStatus || 200;
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(status === 200 ? { text: fixture.transcript || '' } : { detail: { message: 'mock refusal' } }));
}

const server = createServer((req, res) => {
  if (req.url.startsWith('/file/bot')) {
    req.resume();
    req.on('end', () => serveFileDownload(req, res));
    return;
  }
  if (req.url.startsWith('/v1/speech-to-text')) {
    let n = 0;
    req.on('data', (c) => (n += c.length));
    req.on('end', () => serveSpeechToText(req, res, n));
    return;
  }
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
