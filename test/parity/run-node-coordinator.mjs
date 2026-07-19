#!/usr/bin/env node
// Boot the REFERENCE Node coordinator against the local mock Bot API.
//
// SAFETY, three ways:
//   1. coordinator.mjs is COPIED to a scratch dir, so it reads the fake
//      config.json written next to the copy and never the owner's real one.
//      The owner's ~/.claude-remote is only ever read, never written.
//   2. globalThis.fetch is replaced before the import. Anything that is not
//      the mock base URL throws, so a stray request cannot reach Telegram.
//   3. The token in the scratch config is a literal fake.
//
// Usage: node run-node-coordinator.mjs <coordinator.mjs> <scratchDir> <mockBase>

import { copyFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [source, scratchDir, mockBase] = process.argv.slice(2);
mkdirSync(scratchDir, { recursive: true });
const copy = join(scratchDir, 'coordinator.mjs');
copyFileSync(source, copy);

const REAL = 'https://api.telegram.org';
// The voice lane posts to ElevenLabs. It is redirected to the same local mock
// so a parity run never leaves the loopback interface and never spends (or
// even sees) a real speech-to-text key.
const REAL_STT = 'https://api.elevenlabs.io';
const realFetch = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const url = typeof input === 'string' ? input : input.url;
  for (const host of [REAL, REAL_STT]) {
    if (url.startsWith(host)) return realFetch(mockBase + url.slice(host.length), init);
  }
  throw new Error(`parity harness blocked a non-mock request: ${url}`);
};

await import(pathToFileURL(copy).href);
