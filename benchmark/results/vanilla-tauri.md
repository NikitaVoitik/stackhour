# Vanilla TypeScript + Tauri checkpoint

Status: production Vite frontend embedded in Tauri 2.11.5, visually checked,
with five cold and five warm release-binary trials. A single primed Xvfb/D-Bus
session contains the matrix; each cold app trial still receives fresh XDG
config/cache directories. Values are medians of five per-run values.

| Metric | Cold | Warm |
|---|---:|---:|
| Launch → first frame | 190.77 ms | 181.42 ms |
| Launch → tree visible | 218.56 ms | 210.87 ms |
| Project open → tree presented | 25.93 ms | 26.18 ms |
| File click → disk read | 0.64 ms | 0.68 ms |
| File click → text presented | 14.16 ms | 14.10 ms |
| File switch → stable frame | 23.41 ms | 23.90 ms |
| LSP request → response | 1,766.89 ms | 1,866.00 ms |
| LSP response → outline | 7.70 ms | 6.42 ms |

| Scroll metric | Cold | Warm |
|---|---:|---:|
| Median frame | 16.00 ms | 16.00 ms |
| p95 frame | 17.00 ms | 17.00 ms |
| p99 frame | 17.00 ms | 17.00 ms |
| Worst frame | 23.00 ms | 22.00 ms |
| Frames >16.7 ms (of 640) | 52 | 50 |
| Frames >33.3 ms | 0 | 0 |
| Longest main-thread stall | 6.30 ms | 5.30 ms |

| Loaded process group | Cold | Warm |
|---|---:|---:|
| Tauri/WebKitGTK UI/runtime | 534.2 MiB | 533.5 MiB |
| typescript-language-server | 138.3 MiB | 139.0 MiB |
| tsserver (combined) | 556.6 MiB | 581.1 MiB |

The stripped release binary is 8,092,840 bytes and its `.tar.gz` is 2,194,585
bytes. Those sizes exclude the system-provided WebKitGTK 2.50.6 runtime. Raw
events and the accepted screenshot are committed beside this checkpoint.
