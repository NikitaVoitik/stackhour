#!/usr/bin/env node
// MEDIA + VOICE PARITY: boot the reference Node coordinator and the Rust
// bridge against the same local mock Bot API, feed both the same four inbound
// messages, and diff everything the media lane produces.
//
// The four messages, chosen to cover the whole lane:
//   1. a captioned PHOTO            -> downloaded, prompt built from the caption
//   2. an image/png DOCUMENT        -> downloaded, prompt built from the fallback
//   3. an OVERSIZED document        -> rejected by maxMediaBytes, never fetched
//   4. a VOICE note                 -> downloaded, transcribed, echoed, routed
//
// What is compared, side by side:
//   * the prompt text handed to the engine on stdin (mediaPrompt output),
//     with only the generated filename normalised away
//   * every chat message the lane emits (the 👀 reaction, the ⚠️ rejection,
//     the 🎙️ placeholder and the 🎙️ transcript)
//   * where the files land: the media dir, the 0600/0700 modes, which files
//     survive the run and which are unlinked
//   * pruning: a file older than 7 days is swept at boot, a fresh one is not
//
// SAFETY: nothing here may touch api.telegram.org or api.elevenlabs.io. Both
// implementations are pinned to a 127.0.0.1 mock with a fake token and a fake
// speech-to-text key; the Node side additionally runs behind a fetch guard
// that throws on any other URL. The owner's ~/.claude-remote is read only to
// COPY coordinator.mjs into a scratch dir, never written.
//
// Usage: node media-and-voice.mjs [--bin <stackhour binary>]
// Exit code 0 = the lanes match.

import { spawn } from 'node:child_process';
import {
  mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync,
  readdirSync, statSync, utimesSync, chmodSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, '..', '..');
const REFERENCE = process.env.PARITY_REFERENCE ||
  join(process.env.HOME, '.claude-remote', 'coordinator.mjs');

const argv = process.argv.slice(2);
const BIN = (() => {
  const i = argv.indexOf('--bin');
  if (i >= 0) return argv[i + 1];
  return join(REPO, 'target', 'release', 'stackhour');
})();

const CHAT = 4242;
// Obviously fake, both of them. No real credential is ever read by this file.
const TOKEN = '123456:FAKE-TOKEN-FOR-PARITY-TESTS';
const STT_KEY = 'fake-speech-to-text-key';

const ONE_MB = 1024 * 1024;
const MAX_MEDIA_BYTES = ONE_MB; // small, so message 3 trips the cap
const TRANSCRIPT = 'hello from the mock transcriber';

const PHOTO_BYTES = Buffer.from('\xff\xd8\xff\xe0 not really a jpeg', 'binary');
const PNG_BYTES = Buffer.from('\x89PNG not really a png', 'binary');
const OGG_BYTES = Buffer.from('OggS not really an ogg', 'binary');

const msg = (id, extra) => ({
  message_id: id, chat: { id: CHAT }, from: { id: 7, is_bot: false }, ...extra,
});

const UPDATES = [
  { update_id: 1, message: msg(11, {
    caption: 'what is this?',
    photo: [
      { file_id: 'photo-small', file_size: 100, width: 90, height: 90 },
      { file_id: 'photo-big', file_size: PHOTO_BYTES.length, width: 1280, height: 960 },
    ],
  }) },
  { update_id: 2, message: msg(12, {
    document: { file_id: 'doc-png', mime_type: 'image/png', file_name: 'diagram.png', file_size: PNG_BYTES.length },
  }) },
  { update_id: 3, message: msg(13, {
    document: { file_id: 'doc-huge', mime_type: 'video/mp4', file_name: 'huge.mp4', file_size: 5 * ONE_MB },
  }) },
  { update_id: 4, message: msg(14, {
    voice: { file_id: 'voice-1', mime_type: 'audio/ogg', duration: 3, file_size: OGG_BYTES.length },
  }) },
];

const FIXTURE = {
  transcript: TRANSCRIPT,
  files: {
    'photo-big': { file_path: 'photos/file_31.jpg', file_size: PHOTO_BYTES.length, bodyBase64: PHOTO_BYTES.toString('base64') },
    'doc-png': { file_path: 'documents/file_5.png', file_size: PNG_BYTES.length, bodyBase64: PNG_BYTES.toString('base64') },
    // Present so a cap that failed to fire would still get a plausible
    // download rather than a 404 that could be mistaken for the rejection.
    'doc-huge': { file_path: 'videos/file_9.mp4', file_size: 5 * ONE_MB, bodyBase64: '' },
    'voice-1': { file_path: 'voice/file_7.oga', file_size: OGG_BYTES.length, bodyBase64: OGG_BYTES.toString('base64') },
  },
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function startMock(outPath, updatesPath, filesPath) {
  return new Promise((resolve, reject) => {
    const p = spawn(process.execPath, [join(HERE, 'mock-bot-api.mjs'), outPath, updatesPath, filesPath], {
      stdio: ['ignore', 'pipe', 'inherit'],
    });
    p.stdout.on('data', (d) => {
      const m = /PORT (\d+)/.exec(d.toString());
      if (m) resolve({ proc: p, base: `http://127.0.0.1:${m[1]}` });
    });
    p.on('exit', (c) => reject(new Error(`mock exited early (${c})`)));
  });
}

async function stop(proc) {
  proc.kill('SIGTERM');
  await new Promise((r) => proc.on('exit', r));
}

/** A stand-in engine: records the prompt it was handed, then says nothing. */
function writeFakeEngine(dir) {
  const bin = join(dir, 'fake-claude');
  const log = join(dir, 'prompts.log');
  writeFileSync(bin, `#!/bin/sh\nprintf '<<<PROMPT>>>\\n' >> ${log}\ncat >> ${log}\nprintf '\\n' >> ${log}\n`);
  chmodSync(bin, 0o755);
  return { bin, log };
}

const SEVEN_DAYS = 7 * 24 * 60 * 60;

/** Seed the media dir with one stale and one fresh file, for the prune check. */
function seedForPrune(mediaDir) {
  mkdirSync(mediaDir, { recursive: true, mode: 0o700 });
  const stale = join(mediaDir, 'stale-prune-probe.jpg');
  const fresh = join(mediaDir, 'fresh-prune-probe.jpg');
  writeFileSync(stale, 'old');
  writeFileSync(fresh, 'new');
  const old = Date.now() / 1000 - SEVEN_DAYS - 3600;
  utimesSync(stale, old, old);
  return { stale, fresh };
}

/**
 * Run one implementation against a fresh mock.
 * `launch(dir, base, engineBin)` returns the child process; `mediaDirOf(dir)`
 * says where that implementation keeps its attachments.
 */
async function record(name, launch, mediaDirOf) {
  const dir = mkdtempSync(join(tmpdir(), `parity-media-${name}-`));
  const out = join(dir, 'requests.json');
  const updatesPath = join(dir, 'updates.json');
  const filesPath = join(dir, 'files.json');
  writeFileSync(updatesPath, JSON.stringify(UPDATES));
  writeFileSync(filesPath, JSON.stringify(FIXTURE));

  const engine = writeFakeEngine(dir);
  const mediaDir = mediaDirOf(dir);
  const prune = seedForPrune(mediaDir);

  const mock = await startMock(out, updatesPath, filesPath);
  const child = launch(dir, mock.base, engine.bin);
  // Long enough for: boot, one getUpdates carrying all four messages, four
  // downloads, one transcription and four serialized engine runs.
  await sleep(8000);
  child.kill('SIGKILL');
  await new Promise((r) => child.on('exit', r));
  await stop(mock.proc);

  return {
    dir,
    mediaDir,
    requests: JSON.parse(readFileSync(out, 'utf8')),
    prompts: existsSync(engine.log) ? readFileSync(engine.log, 'utf8') : '',
    files: existsSync(mediaDir) ? readdirSync(mediaDir).sort() : [],
    mediaDirMode: existsSync(mediaDir) ? statSync(mediaDir).mode & 0o777 : null,
    // The prune probes are seeded by this harness with its own umask, so only
    // the files the implementation itself wrote are mode-checked.
    fileModes: Object.fromEntries((existsSync(mediaDir) ? readdirSync(mediaDir) : [])
      .filter((f) => !f.endsWith('prune-probe.jpg'))
      .map((f) => [f, statSync(join(mediaDir, f)).mode & 0o777])),
    pruned: { stale: existsSync(prune.stale), fresh: existsSync(prune.fresh) },
  };
}

const config = (base, engineBin) => ({
  token: TOKEN,
  chatId: CHAT,
  defaultTarget: 'gcp',
  maxMediaBytes: MAX_MEDIA_BYTES,
  elevenLabsApiKey: STT_KEY,
  apiRoot: base,                                  // Rust seam; Node uses the fetch guard
  elevenLabsEndpoint: `${base}/v1/speech-to-text`, // ditto
  targets: {
    gcp: { label: '☁️ GCP', type: 'local', cwd: '/tmp', claudeBin: engineBin, codexBin: '/bin/true' },
    mac: { label: '🖥️ Mac', type: 'remote' },
  },
});

async function recordNode() {
  return record(
    'node',
    (dir, base, engineBin) => {
      const scratch = join(dir, 'ref');
      const child = spawn(
        process.execPath,
        [join(HERE, 'run-node-coordinator.mjs'), REFERENCE, scratch, base],
        { stdio: ['ignore', 'ignore', 'inherit'] },
      );
      // config.json must exist next to the copy before the import runs; the
      // runner creates the dir first, so keep trying until it does.
      const cfg = join(scratch, 'config.json');
      const write = () => {
        try { writeFileSync(cfg, JSON.stringify(config(base, engineBin))); }
        catch { setTimeout(write, 5); }
      };
      write();
      return child;
    },
    // The Node coordinator keeps media next to itself, i.e. next to the copy.
    (dir) => join(dir, 'ref', 'media'),
  );
}

async function recordRust() {
  return record(
    'rust',
    (dir, base, engineBin) => {
      writeFileSync(join(dir, 'config.json'), JSON.stringify(config(base, engineBin)));
      const cfgDir = join(dir, 'no-such-config-dir'); // shipped defaults only
      return spawn(BIN, ['bridge', 'coordinator', '--runtime-dir', dir], {
        stdio: ['ignore', 'ignore', 'inherit'],
        env: {
          ...process.env,
          STACKHOUR_CONFIG_DIR: cfgDir,
          STACKHOUR_CONFIG: join(cfgDir, 'config.json'),
          STACKHOUR_DATA: join(dir, 'data'),
        },
      });
    },
    (dir) => join(dir, 'media'),
  );
}

// --- extraction -------------------------------------------------------------

/** `<epochMs>-<uuid>.jpg` -> `<GENERATED>.jpg`, and the media dir -> `<MEDIA>`. */
function normalise(text, mediaDir) {
  return text
    .split(mediaDir).join('<MEDIA>')
    .replace(/\d{13,}-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, '<GENERATED>');
}

/** The prompts the engine was handed, in sorted order (the two lanes may
 *  interleave their downloads differently; the SET is the contract). */
function promptsOf(run) {
  return normalise(run.prompts, run.mediaDir)
    .split('<<<PROMPT>>>\n')
    .slice(1)
    .map((p) => p.replace(/\n$/, ''))
    .sort();
}

/**
 * Every chat text the media lane produced, SORTED.
 *
 * Sorted, not in arrival order, and that is a real divergence rather than
 * harness sloppiness: coordinator.mjs calls handleMediaMessage /
 * handleVoiceMessage WITHOUT awaiting them, so four messages in one
 * getUpdates batch download concurrently and their replies interleave by
 * completion time. The Rust daemon handles the batch sequentially, so its
 * replies always come out in message order. Same messages, same content,
 * same routed prompts; only the emission order of the status replies differs,
 * and the Node's own order is not stable enough to be a contract.
 */
function chatOf(run) {
  return run.requests
    .filter((r) => r.method === 'sendMessage' || r.method === 'editMessageText')
    .map((r) => r.body.text || '')
    .filter((t) => /^(⚠️|🎙️)/.test(t))
    .sort();
}

function reactionsOf(run) {
  return run.requests
    .filter((r) => r.method === 'setMessageReaction')
    .map((r) => `${r.body.message_id}:${(r.body.reaction || []).map((x) => x.emoji).join('')}`)
    .sort();
}

/** Which file_paths were actually fetched from the file API. */
function downloadsOf(run) {
  return run.requests.filter((r) => r.method === 'download').map((r) => r.body.file_path).sort();
}

function sttCallsOf(run) {
  return run.requests.filter((r) => r.method === 'speech-to-text').length;
}

/** The saved attachments, with the generated part of the name normalised. */
function savedOf(run) {
  return run.files
    .filter((f) => !f.endsWith('prune-probe.jpg'))
    .map((f) => f.replace(/^\d{13,}-[0-9a-f-]{36}/i, '<GENERATED>'))
    .sort();
}

// --- run --------------------------------------------------------------------

if (!existsSync(BIN)) {
  console.error(`missing binary: ${BIN}\nbuild it with: cargo build --release`);
  process.exit(2);
}
if (!existsSync(REFERENCE)) {
  console.error(`missing reference coordinator: ${REFERENCE}`);
  process.exit(2);
}

const node = await recordNode();
const rust = await recordRust();

const failures = [];
function cmp(label, x, y) {
  const sx = JSON.stringify(x, null, 2);
  const sy = JSON.stringify(y, null, 2);
  if (sx === sy) { console.log(`ok   ${label}`); return; }
  failures.push(label);
  console.log(`FAIL ${label}\n  node: ${sx}\n  rust: ${sy}`);
}

function want(label, ok, detail) {
  if (ok) { console.log(`ok   ${label}`); return; }
  failures.push(label);
  console.log(`FAIL ${label}${detail ? `\n  ${detail}` : ''}`);
}

if (process.env.PARITY_DUMP) {
  console.log('--- node ---\n' + JSON.stringify({ prompts: promptsOf(node), chat: chatOf(node), files: node.files }, null, 2));
  console.log('--- rust ---\n' + JSON.stringify({ prompts: promptsOf(rust), chat: chatOf(rust), files: rust.files }, null, 2));
}

// Guard against a vacuous pass: the reference must actually have done the work.
const nodePrompts = promptsOf(node);
want('the reference produced two media prompts and one transcript prompt',
  nodePrompts.length === 3, JSON.stringify(nodePrompts, null, 2));
want('the reference downloaded three files',
  downloadsOf(node).length === 3, JSON.stringify(downloadsOf(node)));
want('the reference called speech-to-text once', sttCallsOf(node) === 1);

// 1-4. the prompt text built from each message
cmp('engine prompts (mediaPrompt output + the routed transcript)', nodePrompts, promptsOf(rust));

// The photo prompt must carry the caption, and the document prompt the
// image fallback. Asserted on the Rust side against literal text, so a
// harness that silently compared two empty sets cannot pass.
want('the photo prompt is the caption + the image guidance',
  promptsOf(rust).some((p) => p.startsWith('what is this?\n\nTelegram attachment (image, image/jpeg, telegram-photo.jpg) is saved locally at: <MEDIA>/<GENERATED>.jpg\nUse the available image inspection tool to view it.')),
  JSON.stringify(promptsOf(rust), null, 2));
want('the document prompt uses the image request fallback and the document name',
  promptsOf(rust).some((p) => p === 'Please inspect this image and respond.\n\nTelegram attachment (image, image/png, diagram.png) is saved locally at: <MEDIA>/<GENERATED>.png\nUse the available image inspection tool to view it.'),
  JSON.stringify(promptsOf(rust), null, 2));
want('the transcript is routed verbatim, with no media framing',
  promptsOf(rust).includes(TRANSCRIPT));

// chat surface
cmp('chat messages from the media lane', chatOf(node), chatOf(rust));
cmp('👀 reactions', reactionsOf(node), reactionsOf(rust));
want('the oversized attachment is rejected with the MB message',
  chatOf(rust).includes('⚠️ Could not process attachment: Attachment is too large (5 MB; limit 1 MB).'),
  JSON.stringify(chatOf(rust), null, 2));
want('the transcript is echoed back to the chat',
  chatOf(rust).includes(`🎙️ Transcript:\n${TRANSCRIPT}`), JSON.stringify(chatOf(rust), null, 2));

// where the bytes went
cmp('files fetched from the file API', downloadsOf(node), downloadsOf(rust));
want('the oversized file was never fetched',
  !downloadsOf(rust).includes('videos/file_9.mp4'), JSON.stringify(downloadsOf(rust)));
cmp('speech-to-text calls', sttCallsOf(node), sttCallsOf(rust));
cmp('attachments left on disk', savedOf(node), savedOf(rust));
want('the voice audio is unlinked, the two attachments are kept',
  savedOf(rust).length === 2 && !savedOf(rust).some((f) => f.endsWith('.ogg')),
  JSON.stringify(savedOf(rust)));
cmp('media dir mode', node.mediaDirMode, rust.mediaDirMode);
want('saved attachments are 0600 on both sides',
  Object.values(node.fileModes).every((m) => m === 0o600) &&
  Object.values(rust.fileModes).every((m) => m === 0o600),
  JSON.stringify({ node: node.fileModes, rust: rust.fileModes }));

// pruning
cmp('prune: {stale kept?, fresh kept?}', node.pruned, rust.pruned);
want('prune swept the 7-day-old file and spared the fresh one',
  rust.pruned.stale === false && rust.pruned.fresh === true, JSON.stringify(rust.pruned));

if (failures.length) {
  console.error(`\n${failures.length} media parity failure(s): ${failures.join('; ')}`);
  console.error(`node dir: ${node.dir}\nrust dir: ${rust.dir}`);
  process.exit(1);
}
console.log('\nmedia + voice parity: OK');
