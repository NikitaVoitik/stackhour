# Desktop benchmark findings journal

This is a running, evidence-linked notebook. It intentionally does not select
a winner until all five candidates have been built, visually checked, and run
through the complete cold/warm matrix.

## 2026-07-22 — controlled baseline and Vanilla TypeScript + Electron

### Contract decisions

- The benchmark starts from commit `b545799` on `rust-rewrite`; the immutable
  shared contract is commit `689863a` on `benchmark/common`.
- The fixture contains 5,122 TypeScript files: 5,120 small project files and
  two same-shape 20,000-line switch targets.
- At 1280×800, the required tabs, output panel, outline, and status bar leave
  room for 30 complete 20 px editor lines and 40 complete 18 px tree rows.
  Every candidate must match those counts.
- “Cold” means a fresh application-owned XDG cache/config directory. The host
  kernel page cache is retained because dropping it would require global host
  mutation and would also disturb unrelated workloads. Results must not be
  described as power-on or kernel-cache-cold startup.
- LSP memory is part of the workload but not part of UI/runtime RAM. The raw
  samples classify `typescript-language-server` and each `tsserver` process
  separately.

### Implementation and visual gate

- The initial packaged Electron build displayed only the window background.
  Root-relative Vite asset paths do not load from `file://`; setting Vite's
  production base to `./` fixed the package.
- A sandboxed `.mjs` preload did not expose the benchmark bridge in this
  Electron configuration. A sandboxed CommonJS preload (`preload.cjs`) fixed
  it without disabling context isolation or the renderer sandbox.
- The accepted screenshot is
  `screenshots/vanilla-electron.png`. It shows the same two tabs, selected
  source, virtual tree, 30 source rows, lexical highlighting, populated LSP
  outline, output panel, and status bar required of the other candidates.
- Chromium's GPU process cannot initialize under this host's Xvfb session and
  falls back. That limitation is visible in the run logs and applies to both
  Electron candidates. Tauri and GPUI must be checked for an equivalent
  software-rendering/headless limitation before cross-runtime smoothness is
  interpreted.

### Completed measurements

- `results/raw/vanilla-electron-{cold,warm}-{1..5}.jsonl` contains five valid
  cold and five valid warm production-package runs.
- Each run contains launch, scan, file-read, presented-frame, stable-frame,
  LSP, outline, 640 scroll-frame, longest-stall, and process-classified memory
  events. The 30 file switches are also retained in the raw event stream.
- No comparative conclusion is recorded yet. The numbers become meaningful
  only after the matching Svelte/Electron, Vanilla/Tauri, Svelte/Tauri, and
  GPUI runs exist on this same machine contract.

### Risks to revisit

- The Electron scanner is an equivalent sorted recursive Node implementation;
  Tauri and GPUI will share the Rust scanner. Scanner timings should be
  reported, but frontend conclusions should emphasize presented-frame deltas
  so scanner language does not dominate the UI comparison.
- The current automated scroll cadence uses the platform's presented animation
  callback/tick. The GPUI implementation must use the same 640-step normalized
  path and report actual frame intervals, not merely CPU loop duration.
- Package size will be measured on the runnable release directory and on the
  distributable archive/bundle separately. Comparing an Electron directory to
  a Tauri `.deb` alone would be misleading.

## 2026-07-22 — Svelte + Electron checkpoint

- The Svelte renderer reuses the accepted Electron main process, recursive
  Node scanner, LSP client, fixture, styling, row counts, normalized scroll
  path, and file-switch path. Only DOM ownership/update code changed.
- The accepted `screenshots/svelte-electron.png` matches the Vanilla layout at
  1280×800 and contains a populated 40-row tree, 30 source rows, two tabs,
  outline, output, and status bar.
- The minified production renderer grew from 5.62 kB (2.20 kB gzip) for
  Vanilla to 27.91 kB (11.45 kB gzip) for Svelte. The complete runnable
  Electron directory changed by only 22,288 bytes; the runtime dominates.
- Five cold and five warm runs completed and validated. At this checkpoint,
  Svelte's median launch-to-first-frame is within a few milliseconds of
  Vanilla in both phases. Svelte has somewhat more frames over 16.7 ms in the
  Xvfb scroll workload, but neither Electron candidate produced a >33.3 ms
  scroll frame in its median run.
- Cold LSP round-trip medians differed by roughly 0.47 s even though the LSP
  backend is identical. That is evidence of host/cache variance, not frontend
  causality; LSP comparisons should be reported as workload context rather
  than attributed to Svelte.
- No overall or pairwise winner is selected. The Electron comparison remains
  provisional until the Tauri frontend pair confirms whether the same pattern
  appears under WebKitGTK.
