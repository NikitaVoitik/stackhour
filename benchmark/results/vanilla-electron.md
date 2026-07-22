# Vanilla TypeScript + Electron checkpoint

Status: built as a production Vite bundle inside Electron 37.2.3, visually
checked, and benchmarked in five cold and five warm trials. These are
within-candidate checkpoint statistics, not a winner declaration.

All values below are the median of five per-run values. Raw events are in
`raw/vanilla-electron-{cold,warm}-{1..5}.jsonl`; machine details and the cold
definition are in `machine.json`.

| Metric | Cold | Warm |
|---|---:|---:|
| Launch → first frame | 359.33 ms | 209.20 ms |
| Launch → tree visible | 406.45 ms | 260.83 ms |
| Project open → tree presented | 45.44 ms | 45.44 ms |
| File click → disk read | 2.20 ms | 2.26 ms |
| File click → text presented | 12.62 ms | 19.08 ms |
| File switch → stable frame | 26.55 ms | 33.24 ms |
| LSP request → response | 2,000.56 ms | 1,965.69 ms |
| LSP response → outline | 0.56 ms | 0.49 ms |

| Scroll metric | Cold | Warm |
|---|---:|---:|
| Median frame | 16.50 ms | 16.50 ms |
| p95 frame | 17.40 ms | 17.40 ms |
| p99 frame | 18.70 ms | 18.20 ms |
| Worst frame | 22.40 ms | 24.60 ms |
| Frames >16.7 ms (of 640) | 231 | 228 |
| Frames >33.3 ms | 0 | 0 |
| Longest main-thread stall | 5.70 ms | 7.90 ms |

Loaded process-tree RSS is classified rather than combined:

| Process group | Cold | Warm |
|---|---:|---:|
| Electron UI/runtime | 626.4 MiB | 615.4 MiB |
| typescript-language-server | 139.3 MiB | 137.7 MiB |
| tsserver (combined) | 609.7 MiB | 597.7 MiB |

The runnable release directory is 298,765,646 bytes; its `.tar.gz` is
114,635,289 bytes. Xvfb forced Chromium away from its normal GPU process, so
the frame data describes this controlled headless host and must not be
generalized to a hardware-accelerated desktop without a second run there.
