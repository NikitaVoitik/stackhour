# Svelte + Electron checkpoint

Status: production Svelte 5/Vite renderer inside the same Electron 37.2.3
backend as the Vanilla candidate, visually checked, with five cold and five
warm trials. Values are medians of five per-run values.

| Metric | Cold | Warm |
|---|---:|---:|
| Launch → first frame | 362.61 ms | 206.90 ms |
| Launch → tree visible | 414.13 ms | 253.89 ms |
| Project open → tree presented | 51.78 ms | 45.80 ms |
| File click → disk read | 2.57 ms | 1.95 ms |
| File click → text presented | 16.11 ms | 21.32 ms |
| File switch → stable frame | 27.72 ms | 36.02 ms |
| LSP request → response | 2,474.63 ms | 1,926.15 ms |
| LSP response → outline | 1.41 ms | 1.06 ms |

| Scroll metric | Cold | Warm |
|---|---:|---:|
| Median frame | 16.70 ms | 16.60 ms |
| p95 frame | 18.10 ms | 17.30 ms |
| p99 frame | 20.40 ms | 17.70 ms |
| Worst frame | 25.10 ms | 23.10 ms |
| Frames >16.7 ms (of 640) | 300 | 296 |
| Frames >33.3 ms | 0 | 0 |
| Longest main-thread stall | 8.40 ms | 6.40 ms |

| Loaded process group | Cold | Warm |
|---|---:|---:|
| Electron UI/runtime | 631.7 MiB | 619.1 MiB |
| typescript-language-server | 137.2 MiB | 136.8 MiB |
| tsserver (combined) | 580.0 MiB | 582.7 MiB |

The runnable release directory is 298,787,934 bytes and its `.tar.gz` is
114,643,925 bytes. Raw events and the accepted screenshot are committed next
to this checkpoint.
