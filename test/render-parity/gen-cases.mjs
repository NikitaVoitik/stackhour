#!/usr/bin/env node
// Writes cases.json: the shared corpus of engine outputs both implementations
// are fed. Literal text only, so the Rust test can read the same bytes without
// re-implementing any generation rule.
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));

const longLines = Array.from({ length: 400 }, (_, i) => `line ${i}: the quick brown fox jumps over the lazy dog`).join('\n');
const longNoNewline = 'x'.repeat(9000);
const longTable =
  'Report:\n\n| id | note |\n|---|---|\n' +
  Array.from({ length: 300 }, (_, i) => `| ${i} | ${'note '.repeat(6)}${i} |`).join('\n') +
  '\n\ntrailing prose';

const cases = [
  ['plain-text', 'Deploy finished. Nothing else to report.'],
  ['markdown-bold-code-links', '**Done.** See `src/main.rs` and [the docs](https://example.com/a_b).\n\n*italic* and __underline__ too.'],
  ['fenced-code-with-info-line', 'Here:\n\n```rust\nfn main() { if 1 < 2 && 3 > 2 { println!("hi"); } }\n```\n\ndone'],
  ['markdown-table', 'Results:\n\n| target | engine | status |\n|---|:---:|---:|\n| mac | claude | ok |\n| gcp | codex | failed after 3 retries |\n\nEnd.'],
  ['table-with-ragged-rows-and-emoji', '| a | b |\n| --- | --- |\n| \u{1F680} | x |\n| one | two | three |'],
  ['html-special-characters', 'if a < b && c > d then "quote" & \'apos\' <script>alert(1)</script>\nAT&T &amp; already-escaped'],
  ['html-special-inside-inline-code', 'run `grep -n "a<b" *.rs | head` and `x & y`'],
  ['over-long-message', longLines],
  ['over-long-message-no-newlines', longNoNewline],
  ['over-long-with-table', longTable],
  ['empty', ''],
].map(([name, text]) => ({ name, text }));

writeFileSync(join(HERE, 'cases.json'), JSON.stringify(cases, null, 2) + '\n');
console.error(`wrote ${cases.length} cases`);
