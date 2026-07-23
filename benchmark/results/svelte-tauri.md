# Svelte + Tauri checkpoint

Status: production Svelte 5/Vite renderer embedded in the same Tauri
2.11.5/shared Rust backend as Vanilla Tauri, visually checked, with five cold
and five warm release-binary trials. Values are medians of five per-run values.

| Metric | Cold | Warm |
|---|---:|---:|
| Launch → first frame | 187.90 ms | 187.03 ms |
| Launch → tree visible | 218.63 ms | 219.52 ms |
| Project open → tree presented | 29.17 ms | 29.82 ms |
| File click → disk read | 0.67 ms | 0.66 ms |
| File click → text presented | 14.40 ms | 14.03 ms |
| File switch → stable frame | 28.10 ms | 27.88 ms |
| LSP request → response | 1,808.58 ms | 1,871.07 ms |
| LSP response → outline | 9.89 ms | 6.98 ms |

| Scroll metric | Cold | Warm |
|---|---:|---:|
| Median frame | 16.00 ms | 16.00 ms |
| p95 frame | 17.00 ms | 17.00 ms |
| p99 frame | 20.00 ms | 20.00 ms |
| Worst frame | 25.00 ms | 23.00 ms |
| Frames >16.7 ms (of 640) | 59 | 59 |
| Frames >33.3 ms | 0 | 0 |
| Longest main-thread stall | 8.30 ms | 6.30 ms |

| Loaded process group | Cold | Warm |
|---|---:|---:|
| Tauri/WebKitGTK UI/runtime | 531.1 MiB | 531.8 MiB |
| typescript-language-server | 136.9 MiB | 139.4 MiB |
| tsserver (combined) | 566.9 MiB | 590.3 MiB |

The stripped release binary is 8,101,928 bytes and its `.tar.gz` is 2,202,205
bytes, excluding system WebKitGTK 2.50.6. Raw events and the accepted screenshot
are committed beside this checkpoint.
