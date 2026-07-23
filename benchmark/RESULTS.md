# Desktop UI benchmark results

Completed 2026-07-23 on the machine recorded in
[`results/machine.json`](results/machine.json). All values are medians of five
valid release/production runs per cold/warm phase. Raw JSONL, accepted
screenshots, build code, and checkpoint notes are retained on each candidate
branch.

## Outcome

The controlled 2×2 supports **Svelte + Tauri** as the best product default for
this project. Tauri consistently reduced startup, UI/runtime RSS, process
count, file-presentation latency, and packaged app bytes relative to Electron.
Svelte did not materially increase startup or memory in either runtime. Its
small but repeatable file-switch and scroll-tail cost is a reasonable trade for
component structure and frontend reuse.

**Vanilla TypeScript + Tauri** is the measured latency choice inside the 2×2.
Choose it only if the few-millisecond update advantage is worth maintaining a
frameworkless UI.

GPUI is a meaningful memory and first-frame baseline, but not a demonstrated
interaction-performance winner here. It used about 102 MiB idle and 306 MiB
loaded UI/runtime RSS, versus roughly 412/534 MiB for Vanilla Tauri and
479/615 MiB for Vanilla Electron in warm runs. Its first frame was 88 ms. But
GPUI required a nested software Wayland compositor on this headless host, and
its measured presentation and scroll path was much slower. That non-equivalent
graphics path prevents a fair accelerated-smoothness conclusion. On the
evidence available, the improvement is not large or complete enough to justify
moving this UI to desktop-only GPUI.

## Visual and workload gate

All five candidates were built in release/production mode and visually checked
before comparative interpretation. Each accepted image shows the same:

- 1280×800 content at DPR 1 and 96 DPI;
- Noto Sans/Noto Sans Mono at 13 px;
- `src/selected.ts` initially selected from the 5,122-file fixture;
- 40 visible tree rows and 30 visible source lines, with overscan 4;
- two tabs, outline, output panel, status bar, and equal lexical highlighting;
- fixed-height tree/editor windowing, 640 identical scroll steps, and 30 file
  switches between equal-shape 20,000-line files;
- `typescript-language-server --stdio` with the same initialize, didOpen, and
  documentSymbol behavior. Tauri and GPUI share the Rust backend; Electron
  uses the equivalent Node scanner and byte-equivalent JSON-RPC sequence.

Accepted evidence:

| Candidate | Branch and checkpoint | Visual gate | Raw runs |
|---|---|---|---|
| Vanilla TS + Electron | [`benchmark/vanilla-electron` @ `d4e4aad`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/vanilla-electron) | [`vanilla-electron.png`](https://github.com/NikitaVoitik/stackhour/blob/benchmark/vanilla-electron/benchmark/screenshots/vanilla-electron.png) | [`raw/`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/vanilla-electron/benchmark/results/raw) |
| Svelte + Electron | [`benchmark/svelte-electron` @ `514f88a`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/svelte-electron) | [`svelte-electron.png`](https://github.com/NikitaVoitik/stackhour/blob/benchmark/svelte-electron/benchmark/screenshots/svelte-electron.png) | [`raw/`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/svelte-electron/benchmark/results/raw) |
| Vanilla TS + Tauri | [`benchmark/vanilla-tauri` @ `6b2f9af`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/vanilla-tauri) | [`vanilla-tauri.png`](https://github.com/NikitaVoitik/stackhour/blob/benchmark/vanilla-tauri/benchmark/screenshots/vanilla-tauri.png) | [`raw/`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/vanilla-tauri/benchmark/results/raw) |
| Svelte + Tauri | [`benchmark/svelte-tauri` @ `07fbba7`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/svelte-tauri) | [`svelte-tauri.png`](https://github.com/NikitaVoitik/stackhour/blob/benchmark/svelte-tauri/benchmark/screenshots/svelte-tauri.png) | [`raw/`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/svelte-tauri/benchmark/results/raw) |
| GPUI | [`benchmark/gpui` @ `b7d845b`](https://github.com/NikitaVoitik/stackhour/tree/benchmark/gpui) | [`gpui.png`](screenshots/gpui.png) | [`raw/`](results/raw) |

## 1. Direct 2×2 result

### Required interaction timings

Milliseconds; each cell is **cold / warm**.

| Timing | Vanilla Electron | Svelte Electron | Vanilla Tauri | Svelte Tauri |
|---|---:|---:|---:|---:|
| App launch → first frame | 359.33 / 209.20 | 362.61 / 206.90 | 190.77 / 181.42 | 187.90 / 187.03 |
| App launch → tree visible | 406.45 / 260.83 | 414.13 / 253.89 | 218.56 / 210.87 | 218.63 / 219.52 |
| App launch → usable stable frame | 433.12 / 292.59 | 441.97 / 290.04 | 249.08 / 240.19 | 254.60 / 254.09 |
| Project-open request → tree presented | 45.44 / 45.44 | 51.78 / 45.80 | 25.93 / 26.18 | 29.17 / 29.82 |
| File click → disk read complete | 2.20 / 2.26 | 2.57 / 1.95 | 0.64 / 0.68 | 0.67 / 0.66 |
| File click → text in presented frame | 12.62 / 19.08 | 16.11 / 21.32 | 14.16 / 14.10 | 14.40 / 14.03 |
| File switch → stable frame | 26.55 / 33.24 | 27.72 / 36.02 | 23.41 / 23.90 | 28.10 / 27.88 |
| LSP request → response | 2,000.56 / 1,965.69 | 2,474.63 / 1,926.15 | 1,766.89 / 1,866.00 | 1,808.58 / 1,871.07 |
| LSP response → outline presented | 0.56 / 0.49 | 1.41 / 1.06 | 7.70 / 6.42 | 9.89 / 6.98 |

The LSP round-trip variation is dominated by starting and initializing the same
TypeScript language processes, not by frontend work. Frontend comparisons
should use the presentation rows, especially response → outline.

### Smoothness

Each run contains 640 presented-frame intervals. Cells are **cold / warm**
medians of the five per-run statistics.

| Scroll metric | Vanilla Electron | Svelte Electron | Vanilla Tauri | Svelte Tauri |
|---|---:|---:|---:|---:|
| Median frame, ms | 16.50 / 16.50 | 16.70 / 16.60 | 16.00 / 16.00 | 16.00 / 16.00 |
| p95, ms | 17.40 / 17.40 | 18.10 / 17.30 | 17.00 / 17.00 | 17.00 / 17.00 |
| p99, ms | 18.70 / 18.20 | 20.40 / 17.70 | 17.00 / 17.00 | 20.00 / 20.00 |
| Worst, ms | 22.40 / 24.60 | 25.10 / 23.10 | 23.00 / 22.00 | 25.00 / 23.00 |
| Frames >16.7 ms | 231 / 228 | 300 / 296 | 52 / 50 | 59 / 59 |
| Frames >33.3 ms | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| Longest main-thread/frame stall, ms | 5.70 / 7.90 | 8.40 / 6.40 | 6.30 / 5.30 | 8.30 / 6.30 |

Svelte produced more frames just over the 16.7 ms threshold in both runtime
pairs, but none of the 2×2 candidates exceeded 33.3 ms. Vanilla had the lower
file-switch median in all four cold/warm comparisons.

### Memory and processes

MiB RSS; values are **cold / warm**. UI/runtime excludes
`typescript-language-server` and `tsserver`.

| Metric | Vanilla Electron | Svelte Electron | Vanilla Tauri | Svelte Tauri |
|---|---:|---:|---:|---:|
| Idle UI/runtime RSS | 490.0 / 479.2 | 492.6 / 478.3 | 410.4 / 412.1 | 411.7 / 417.2 |
| Loaded UI/runtime RSS | 626.4 / 615.4 | 631.7 / 619.1 | 534.2 / 533.5 | 531.1 / 531.8 |
| Loaded UI/runtime processes | 5 / 5 | 5 / 5 | 4 / 4 | 4 / 4 |
| Loaded child processes, including LSP | 7 / 7 | 7 / 7 | 6 / 6 | 6 / 6 |
| Loaded language-server RSS | 139.3 / 137.7 | 137.2 / 136.8 | 138.3 / 139.0 | 136.9 / 139.4 |
| Loaded tsserver RSS | 609.7 / 597.7 | 580.0 / 582.7 | 556.6 / 581.1 | 566.9 / 590.3 |

Svelte's differences are within about 6 MiB and a few milliseconds. There is
no evidence of a material Svelte startup or RAM penalty in this workload.
Tauri reduced warm loaded UI/runtime RSS by about 82–87 MiB and used one fewer
UI/runtime process than the matching Electron candidate.

### Packaged application size

| Candidate | Runnable/binary bytes | Binary/directory gzip bytes | Important exclusion |
|---|---:|---:|---|
| Vanilla Electron | 298,765,646 | 114,635,289 | Bundles Electron/Chromium |
| Svelte Electron | 298,787,934 | 114,643,925 | Bundles Electron/Chromium |
| Vanilla Tauri | 8,092,840 | 2,194,585 | System WebKitGTK 2.50.6 excluded |
| Svelte Tauri | 8,101,928 | 2,202,205 | System WebKitGTK 2.50.6 excluded |

Svelte added only 22,288 bytes to the Electron directory and 9,088 bytes to
the Tauri binary. Tauri's distributable app payload is dramatically smaller,
but this is not a claim that the operating system webview occupies zero disk.

### Answers: TypeScript versus Svelte

- **Startup and RAM:** no material Svelte increase. Differences change sign by
  runtime and cache phase and remain small.
- **Large file-list render:** Vanilla was modestly faster. Project-open → tree
  was 0.36–6.34 ms lower in Electron and 3.0–3.6 ms lower in Tauri.
- **File switch/update latency:** Vanilla was consistently lower by roughly
  1.2–4.7 ms for a stable switch.
- **Long frames:** Svelte had more frames over 16.7 ms, especially under
  Electron, but neither frontend produced a frame over 33.3 ms in this 2×2.

### Answers: Electron versus Tauri

- Tauri reached the first frame about 10–13% sooner warm and 47–48% sooner
  cold. It reached the initial usable stable frame 12–18% sooner warm.
- Tauri used about 13–14% less warm loaded UI/runtime RSS and one fewer runtime
  process.
- Tauri presented file opens and stable switches sooner in the warm workload.
- Tauri recorded substantially fewer scroll frames over 16.7 ms on this host.
- Tauri's binary-only package was about 8.1 MB versus Electron's roughly
  299 MB runnable directory, subject to the system-webview caveat above.

## 2. Five-way comparison with GPUI

### Native baseline

| Warm metric | Vanilla Electron | Svelte Electron | Vanilla Tauri | Svelte Tauri | GPUI |
|---|---:|---:|---:|---:|---:|
| First frame, ms | 209.20 | 206.90 | 181.42 | 187.03 | **88.24** |
| Tree visible, ms | 260.83 | 253.89 | 210.87 | 219.52 | 176.16 |
| Usable stable frame, ms | 292.59 | 290.04 | **240.19** | 254.09 | 429.41 |
| File click → presented, ms | 19.08 | 21.32 | 14.10 | **14.03** | 168.33 |
| File switch → stable, ms | 33.24 | 36.02 | **23.90** | 27.88 | 253.05 |
| Idle UI/runtime RSS, MiB | 479.2 | 478.3 | 412.1 | 417.2 | **102.0** |
| Loaded UI/runtime RSS, MiB | 615.4 | 619.1 | 533.5 | 531.8 | **305.6** |
| UI/runtime processes | 5 | 5 | 4 | 4 | **2** |
| Median / p95 scroll, ms | 16.5 / 17.4 | 16.6 / 17.3 | **16.0 / 17.0** | **16.0 / 17.0** | 83.9 / 89.4 |
| Frames >16.7 / >33.3 | 228 / 0 | 296 / 0 | **50 / 0** | 59 / 0 | 639 / 639 |
| Binary/directory bytes | 298.8 MB | 298.8 MB | **8.09 MB** | 8.10 MB | 16.21 MB |
| Gzip bytes | 114.64 MB | 114.64 MB | **2.19 MB** | 2.20 MB | 6.70 MB |

GPUI's 88 ms first frame and much lower RSS are meaningful. Loaded GPUI
UI/runtime RSS was about 43% below Vanilla Tauri and 50% below Vanilla
Electron; idle RSS was about 75–79% lower. It did not, however, reach a usable
initial editor sooner in this harness, and it did not demonstrate a smoothness
advantage.

The graphics caveat is decisive: direct GPUI/X11 under Xvfb created the window
but could not produce observable Vulkan-presented pixels or callbacks. The
saved GPUI runs use software Vulkan inside Weston/X11's software compositor,
hosted by Xvfb. Electron fell back from its GPU process, and Tauri required
WebKitGTK software compositing, but neither uses GPUI's extra nested compositor.
Therefore the GPUI scroll and presentation numbers are valid for reproducing
this CI host, not for ranking physical-GPU desktop rendering.

### Is native work justified?

Not yet. GPUI gains lower memory, fewer processes, a faster first frame, direct
Rust integration, and control over a GPU-native editor renderer. It also
requires a separate desktop UI implementation and is explicitly pre-1.0 with
frequent breaking changes in its [official README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md).
The upstream Zed project currently lists desktop downloads and tracks web as
not yet available in its [repository README](https://github.com/zed-industries/zed).

Tauri is close enough on the robust metrics and clearly ahead on the measured
usable/presentation path. More importantly, Tauri 2 officially targets desktop
plus iOS and Android and describes reusing one UI codebase across those
platforms in its [2.0 release documentation](https://v2.tauri.app/blog/tauri-20/).
The Svelte frontend is ordinary compiled HTML/CSS/JavaScript—the official
[Svelte site](https://svelte.dev/) describes its browser-oriented compiler—so
the view layer can also be reused for a web build when native bridge calls are
kept behind an adapter. Electron similarly preserves web frontend skills but
officially targets Windows, macOS, and Linux desktop
([Electron introduction](https://www.electronjs.org/docs/latest/)). GPUI does
not currently provide an equivalent web or mobile route.

Recommendation: continue with **Svelte + Tauri**. Retain `benchmark/gpui` as a
native baseline and repeat the five-way scroll/presentation suite on an
accelerated physical Linux desktop before reconsidering GPUI for performance.

## Responsiveness during scan and LSP initialization

The event order is consistent across candidates: first frame is presented
before project scanning; tree and initial file are presented before the LSP
request; outline presentation is separately timed after the LSP response.
Scanning and LSP I/O run outside the renderer's UI update loop and do not block
the first visible frame. Scanner memory remains in its backend runtime group;
the separate language-server processes are excluded from UI/runtime RSS. The
launch → tree, launch → usable, LSP round-trip, and response → outline rows
quantify those boundaries.

One limitation remains: this version of the protocol does not inject a second
interactive input while scanning or while `tsserver` initializes. It therefore
demonstrates asynchronous presentation ordering but does not provide a numeric
concurrent-input latency for those two windows. Likewise, the recorded longest
stall belongs to the automated scroll workload. A future harness revision
should add a periodic input/ack heartbeat during scan and LSP initialization.

## Interpretation limits

- Cold means a fresh application-owned XDG cache/config directory; the kernel
  page cache was retained. These are not power-on-cold numbers.
- The machine is a four-vCPU Debian cloud host with Xvfb. No result should be
  generalized to macOS, Windows, mobile, or a physical GPU without repetition.
- Electron, WebKitGTK, and GPUI use different software-rendering fallbacks on
  this host. The 2×2 remains useful, but cross-engine smoothness has more
  uncertainty than timing, memory, process, and package results.
- Tauri and GPUI size figures exclude system graphics/webview libraries;
  Electron bundles Chromium and Node.
- Five trials meet the declared minimum and expose large effects, but small
  differences should not be treated as statistically conclusive.
- The LSP process sometimes changes RSS after initialization; UI/runtime,
  `typescript-language-server`, and combined `tsserver` are always reported
  separately.

The complete implementation diary, including failed visual paths and why each
harness choice was made, is in [`FINDINGS.md`](FINDINGS.md). Per-candidate
checkpoint tables live under [`results/`](results/).
