# Stackhour desktop UI benchmark

This directory is the common contract for five release-build candidates, all
branched from `rust-rewrite`:

| Branch | Frontend | Shell |
|---|---|---|
| `benchmark/vanilla-electron` | frameworkless TypeScript DOM | Electron |
| `benchmark/svelte-electron` | Svelte | Electron |
| `benchmark/vanilla-tauri` | frameworkless TypeScript DOM | Tauri 2 |
| `benchmark/svelte-tauri` | Svelte | Tauri 2 |
| `benchmark/gpui` | native Rust | GPUI |

No branch is a designated winner. `benchmark/results` is populated only after
all five release builds pass the visual contract and complete the same run
matrix.

## Controlled contract

- Linux x86_64, one otherwise-idle host, Xvfb at 1280x800 and 96 DPI.
- Window content is 1280x800, DPR 1, with Noto Sans Mono 13 px for source and
  Noto Sans 13 px for chrome.
- Deterministic fixture: 5,122 TypeScript files. `src/selected.ts` is selected
  at launch and contains 20,000 source lines.
- Exactly 40 project-tree rows and 30 editor source rows are visible. Both
  lists use fixed-height windowing with overscan 4 (48 and 38 mounted rows).
- Identical tabs, outline, output panel, and status bar; identical lexical
  token classes (keyword, string, number, comment, type, function).
- The scroll workload is 480 requestAnimationFrame/tick steps: 160 down, 160
  up, repeated once, with the same normalized easing and scroll range.
- The switch workload alternates `src/selected.ts` and
  `src/alternate.ts` 30 times. Stable frame means two presented frames after
  the requested file's virtual rows are painted.
- The LSP workload uses `typescript-language-server --stdio` and the same JSON
  RPC sequence: initialize, initialized, didOpen, documentSymbol. Runtime/UI
  RSS is sampled separately from the language-server and tsserver process
  trees.
- Rust candidates share `benchmark-core`. Electron uses the equivalent Node
  scanner and byte-for-byte JSON-RPC request bodies.

## Telemetry contract

Every candidate emits one JSON object per line with a monotonic nanosecond
timestamp, candidate, run id, phase (`cold` or `warm`), and event. Required
events are:

- `process_start`, `first_frame`, `project_tree_visible`
- `project_open_requested`, `project_scan_complete`, `tree_presented`
- `file_click`, `disk_read_complete`, `text_presented`, `stable_frame`
- `lsp_request`, `lsp_response`, `outline_presented`
- `frame`, `main_thread_stall`, and `memory_sample`

`scripts/analyze.mjs` rejects incomplete runs rather than silently comparing
different workloads.

## Running

Generate the ignored fixture once with `pnpm fixture`. Each candidate branch
defines `pnpm build`, `pnpm visual`, and `pnpm bench`. The benchmark command
runs at least five cold and five warm release-build trials and writes raw JSONL
under `results/raw`. The common report command consumes the collected files.

Cold trials drop only application-owned caches and use a fresh XDG directory;
they do not claim to be kernel page-cache cold unless run with elevated cache
flushing enabled. Warm trials reuse the XDG directory and immediately follow a
priming launch. This distinction is recorded in the machine manifest.
