# Developing new benchmark candidates

This guide explains how to add more tech-stack candidates to the controlled
desktop-UI benchmark and produce comparable results. It is written so a coding
agent (Codex/Claude) or a human can build each one on an idle machine.

Everything a candidate must hold constant lives in [`contract.json`](contract.json)
and [`README.md`](README.md); read those first. This document is the *how-to*
layered on top.

---

## Planned new candidates

Build these as new branches. Each one keeps the exact same UI, fixture, workload
and telemetry — only the stack underneath changes.

| Branch | Frontend | Shell | Template to copy | Notes |
|---|---|---|---|---|
| `benchmark/react-electron` | React 19 | Electron | `benchmark/svelte-electron` | React is the missing mainstream frontend |
| `benchmark/react-tauri` | React 19 | Tauri 2 | `benchmark/svelte-tauri` | React on the lighter shell |
| `benchmark/solid-tauri` | SolidJS | Tauri 2 | `benchmark/svelte-tauri` | fine-grained reactivity vs Svelte |
| `benchmark/egui` | native `egui`/`eframe` | native (Rust) | `benchmark/gpui` | 2nd native point next to GPUI |

Already on GitHub (do not rebuild): `benchmark/vanilla-electron`,
`benchmark/svelte-electron`, `benchmark/vanilla-tauri`, `benchmark/svelte-tauri`,
`benchmark/gpui`, plus two alternative Electron loaders
`benchmark/vanilla-electron-appproto` and `benchmark/svelte-electron-appproto`
(see "Two Electron loading approaches" below).

Optional stretch candidates if you want to go wider: `vue-tauri`, `qt-qml`
(native C++/Qt), `iced` (native Rust), `dioxus-desktop`, `wails` (Go).

---

## Ground rules (identical for every candidate)

Copy these from the nearest sibling branch; do not re-derive them.

- **Window / render:** 1280×800 content, DPR 1, 96 DPI. Noto Sans 13px chrome,
  Noto Sans Mono 13px code, 20px line height.
- **Fixture (shared, deterministic):** `pnpm fixture` generates 5,121 TS files
  (80 dirs × 64 + `src/selected.ts` [20,000 lines] + `src/alternate.ts`). Never
  edit it by hand; it is gitignored and regenerated.
- **Virtualization:** 40 tree rows, 42 editor rows, overscan 4, tree row 18px,
  editor line 20px. Fixed-height windowing via `translateY`.
- **Identical UI:** same layout and class names as the existing candidates
  (`shell / activity / sidebar / tree / tree-content / tabs / editor /
  editor-content / outline / outline-list / output / log / status`, rows
  `tree-row` / `code-row` / `line-no` / `code`, token classes
  `kw / str / num / comment / type / fn`). Native candidates reproduce the same
  visual layout, not the DOM.
- **Workload:** scroll = 4 legs × 160 rAF steps (**640 `frame` events**, cosine
  easing, `editorTop→19958`, `treeTop→5080`); switch = **30** alternations of
  `selected.ts`/`alternate.ts`; LSP = `typescript-language-server --stdio`,
  `textDocument/documentSymbol` on `selected.ts`. Native/Tauri share the Rust
  `crates/benchmark-core`; Electron uses the equivalent Node scanner and
  byte-identical JSON-RPC bodies.
- **Telemetry:** one JSON object per line — monotonic ns timestamp, `candidate`,
  `phase` (`cold`/`warm`), `runId`, `event`. Required events:
  `process_start, first_frame, project_tree_visible, project_open_requested,
  project_scan_complete, tree_presented, file_click, disk_read_complete,
  text_presented, stable_frame, lsp_request, lsp_response, outline_presented,
  frame, main_thread_stall, memory_sample, benchmark_complete`. Set
  `candidate` to the new branch's stack name.
- **Runs:** a warm prime launch, then **5 cold + 5 warm** release/production
  builds. `scripts/analyze.mjs` rejects any run missing a required event, and
  demands ≥5 valid runs per `candidate:phase` group.

---

## Repo layout a candidate uses

- Web frontend: `benchmark/src/` (+ `benchmark/index.html`, `benchmark/vite.config.*`).
- Electron shell: `benchmark/electron/{main,preload,backend}.mjs`.
- Tauri shell: `benchmark/src-tauri/` (Rust `main.rs`, `tauri.conf.json`,
  `capabilities/`, `icons/`).
- Native shell: its own crate dir, e.g. `benchmark/gpui/` → make `benchmark/egui/`.
- Shared: `benchmark/crates/benchmark-core/` (Rust scanner + LSP client + memory
  classifier), `benchmark/scripts/{generate-fixture,analyze,machine-manifest,
  report-stats}.mjs`, per-shell runners `scripts/run-*.mjs`, visual capture
  `scripts/visual-*.sh`.
- `package.json` scripts per candidate: `build`, `bench`, `visual`, `analyze`,
  `manifest`, `fixture`. The bench/build differ by shell (see recipes).

All candidate branches start from the shared contract commit; the simplest path
is to branch from the nearest sibling that already implements your shell.

---

## Recipes

### React + Electron (`react-electron`, from `svelte-electron`)
1. `git switch -c benchmark/react-electron origin/benchmark/svelte-electron`
2. Replace the Svelte frontend in `src/` with React: add `react`, `react-dom`,
   `@vitejs/plugin-react`; swap the vite plugin; reimplement the UI in
   `src/App.tsx` mounted from `src/main.tsx`. Keep `index.html`, `style.css`,
   the same class names and the same telemetry call order.
3. In `electron/main.mjs` set `candidate = "react-electron"`. Rename the
   packaged artifact in `package.json` (`StackhourBenchReactElectron`) and in
   `scripts/run-electron.mjs` (executable path + `react-electron-*` log prefix).
4. Keep the Electron `build`/`bench` scripts otherwise unchanged.

### React + Tauri (`react-tauri`, from `svelte-tauri`)
1. `git switch -c benchmark/react-tauri origin/benchmark/svelte-tauri`
2. Swap the Svelte frontend for React as above (`vite.config.ts` uses
   `@vitejs/plugin-react`). Leave `src-tauri/` and `crates/benchmark-core` alone.
3. Set the candidate name in the Tauri Rust side / wherever telemetry is stamped;
   rename the `run-tauri.mjs` log prefix to `react-tauri`.
4. `build`: `vite build && tauri build --no-bundle`. `bench`:
   `xvfb-run -a -s '-screen 0 1280x800x24 -dpi 96' dbus-run-session -- node scripts/run-tauri.mjs`.

### SolidJS + Tauri (`solid-tauri`, from `svelte-tauri`)
Same as React+Tauri but use `solid-js` + `vite-plugin-solid`; reimplement the UI
with Solid's fine-grained signals. Log prefix `solid-tauri`.

### egui native (`egui`, from `gpui`)
1. `git switch -c benchmark/egui origin/benchmark/gpui`
2. Add `benchmark/egui/` (Cargo bin crate) using `eframe`/`egui`; depend on
   `crates/benchmark-core` for the scanner, LSP client, process walk and memory
   classifier — reuse it exactly as `gpui/` does. Reproduce the same visual
   layout and emit the same telemetry sequence.
3. Add `scripts/run-egui.mjs` (copy `run-gpui.mjs`) and `scripts/visual-egui.sh`.
   `package.json` `build`: `cargo build --manifest-path egui/Cargo.toml --release`.
4. Headless GPU: reuse `scripts/with-gpui-compositor.sh` (nested software Wayland
   compositor) or run under Xvfb; see gotchas.

---

## Two Electron loading approaches (both valid — pick one, note which)

Electron under a headless/no-GPU host will hang unless you handle two things:
the ES-module renderer script won't run over `file://`, and an ESM preload won't
load in a sandboxed renderer. The shipped branches and the `-appproto` branches
solve it differently — either is fine, just record which you used:

- **Shipped (`svelte-electron` etc.):** CommonJS preload (`preload.cjs`) with
  `sandbox: true`, plain `window.loadFile(dist/index.html)`, and a non-module
  build so `file://` can execute the script.
- **`-appproto` variant:** serve the built renderer over a custom `app://`
  standard-scheme origin (`protocol.handle`), `window.loadURL("app://…")`,
  `sandbox: false` with an ESM `.mjs` preload. `vite.config` keeps `base: "./"`.

Both need `--disable-gpu` and a shown, non-throttled window
(`show: true`, `backgroundThrottling: false`) so `requestAnimationFrame` fires.

---

## Toolchain

- **All:** Node 22, pnpm 10, `typescript-language-server` (dev dep).
- **Electron:** install **hoisted** (`pnpm install --config.node-linker=hoisted`)
  or `electron-packager`'s prune fails on pnpm's symlinked layout; `@electron/asar`
  must be present for packaging.
- **Tauri 2:** Rust stable + `webkit2gtk-4.1` (Linux) / WebKit (macOS),
  `@tauri-apps/cli`.
- **Native (egui/gpui):** Rust stable, Vulkan/GL. On a real GPU host you get
  hardware Vulkan; on a headless CI host use the nested software Wayland
  compositor script or Xvfb + llvmpipe.
- The AWS `nightgame-dev` box is already provisioned with all of the above. On a
  Mac, install the equivalents (Xcode CLT, `rustup`, `pnpm`, and WebKit is
  system-provided for Tauri).

---

## Validate and record results (definition of done)

From `benchmark/`:

1. `pnpm install --config.node-linker=hoisted` (Electron) or `pnpm install`.
2. `pnpm fixture` (once per machine — shared across candidates).
3. `pnpm build` — release/production build.
4. `pnpm bench` — prime + 5 cold + 5 warm; writes `results/raw/<candidate>-<phase>-<n>.jsonl`.
5. `pnpm analyze` — **must print `validated 10 runs …`** with no throw. If a run
   stalls at only `process_start`, the renderer never executed — fix the load
   path before trusting anything.
6. `pnpm visual` — capture `screenshots/<candidate>.png` and eyeball it against
   the reference UI.
7. Commit on the candidate branch: source, `results/raw/*.jsonl`,
   `results/<candidate>.md` (median table — copy an existing candidate's format),
   `screenshots/<candidate>.png`. `scripts/report-stats.mjs` aggregates medians
   across branches for the combined report.

**Benchmark on an otherwise-idle host.** Timings are only valid with nothing else
running — never benchmark while other candidates are still building. Develop in
parallel if you like, but run the timed matrix serially, one candidate at a time.

---

## Ready-to-paste Codex prompt (per candidate)

> Develop the `<frontend>-<shell>` candidate for the Stackhour desktop-UI
> benchmark in this repo (`benchmark/`). Branch `benchmark/<frontend>-<shell>`
> from `origin/benchmark/<template-sibling>`. Keep the benchmark contract
> byte-identical (see `benchmark/DEVELOPING-CANDIDATES.md` and `contract.json`):
> same 1280×800 UI, same class names/layout, same fixture, same virtualization
> (40 tree / 42 editor rows, overscan 4), same workload (640 scroll frames, 30
> file switches, LSP documentSymbol), same telemetry events and order, `candidate`
> set to `<frontend>-<shell>`. Replace only the framework: <specifics>. Preserve
> the shared `crates/benchmark-core`, the runner, and the headless fixes. Then
> `pnpm install`, `pnpm build`, `pnpm bench`, `pnpm analyze` — analyze must print
> "validated 10 runs" — capture a screenshot, write `results/<candidate>.md`, and
> commit source + results + screenshot on the candidate branch. Honor the
> environment gotchas in DEVELOPING-CANDIDATES.md (hoisted install, @electron/asar,
> the Electron headless loader, `--disable-gpu`, no self-matching `pkill`). Run
> the timed benchmark only on an otherwise-idle host.
