#!/usr/bin/env node
// Emits the Telegram payload sequence the NODE coordinator produces for each
// case in cases.json.
//
// SAFETY: coordinator.mjs is never imported. Importing it would start a second
// getUpdates long-poller against the owner's live bot token and steal his
// messages. Instead this reads the file as TEXT, slices out the
// "rendering + tables" section (esc .. deliverFinal), and evaluates that slice
// with stubbed transport functions that record payloads instead of sending
// them. No token, no network, no config.json is touched.
//
// Usage: node node-reference.mjs [path-to-coordinator.mjs] > golden.json

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const COORD = process.argv[2] || '/home/nikita/.claude-remote/coordinator.mjs';

const src = readFileSync(COORD, 'utf8');
function cut(startMarker, endMarker) {
  const from = src.indexOf(startMarker);
  const to = src.indexOf(endMarker, from + 1);
  if (from < 0 || to < 0 || to <= from) {
    throw new Error(`could not locate ${startMarker} .. ${endMarker} in ${COORD}`);
  }
  return src.slice(from, to);
}
// The transport wrappers (sendMessage/deleteMessage/sendRich, including
// sendRich's drop-the-keyboard retry) and the rendering section, verbatim.
const slice =
  cut('async function sendMessage(text, parse_mode', '// ---------- inbound media') +
  cut('function esc(s)', 'function label(name)');
for (const needed of ['sendRich', 'renderHtml', 'hasTable', 'asciiTables', 'deliverFinal']) {
  if (!slice.includes(needed)) throw new Error(`slice is missing ${needed}`);
}

const KB = { inline_keyboard: [[{ text: '\u{1F195} New session', callback_data: 'new' }]] };

// The ONLY thing stubbed is the wire: `tg()` records the method and body it
// was asked to POST and answers the way a standard Bot API would (no
// sendRichMessage method -> null), so sendRich's real retry logic runs.
const prelude = `
const chatId = CHAT;
const controlKb = () => KB;
async function tg(method, body) {
  calls.push({ method, body });
  if (method === 'sendRichMessage') return RICH_OK ? { message_id: 1 } : null;
  if (method === 'deleteMessage') return true;
  return { message_id: 1 };
}
`;

// Build the reference module once from the sliced coordinator source.
const build = new Function(
  'calls',
  'KB',
  'CHAT',
  'RICH_OK',
  `${prelude}\n${slice}\nreturn { deliverFinal, renderHtml, hasTable, asciiTables, esc };`,
);

const CHAT = 4242;
const cases = JSON.parse(readFileSync(join(HERE, 'cases.json'), 'utf8'));
const out = [];

for (const c of cases) {
  const recorded = [];
  const api = build(recorded, KB, CHAT, false);
  await api.deliverFinal(c.text, 77);
  out.push({
    name: c.name,
    calls: recorded,
    // Intermediate values, so a mismatch points at the exact stage.
    has_table: api.hasTable(c.text),
    ascii_tables: api.hasTable(c.text) ? api.asciiTables(c.text) : null,
  });
}

process.stdout.write(JSON.stringify(out, null, 2) + '\n');
