# GPUI native Rust checkpoint

Status: production GPUI 0.2.2 release binary using the shared Rust scanner,
LSP client, process walk, and memory classifier. The candidate was visually
checked and completed five cold and five warm trials. Values are medians of
five per-run values.

| Metric | Cold | Warm |
|---|---:|---:|
| Launch → first frame | 88.58 ms | 88.24 ms |
| Launch → tree visible | 327.39 ms | 176.16 ms |
| Launch → usable stable frame | 577.63 ms | 429.41 ms |
| Project open → tree presented | 237.78 ms | 84.59 ms |
| File click → disk read | 0.68 ms | 0.58 ms |
| File click → text presented | 168.30 ms | 168.33 ms |
| File switch → stable frame | 249.98 ms | 253.05 ms |
| LSP request → response | 3,060.53 ms | 2,984.35 ms |
| LSP response → outline | 104.53 ms | 107.64 ms |

| Scroll metric | Cold | Warm |
|---|---:|---:|
| Median frame | 83.81 ms | 83.88 ms |
| p95 frame | 88.99 ms | 89.36 ms |
| p99 frame | 112.61 ms | 114.45 ms |
| Worst frame | 159.50 ms | 134.80 ms |
| Frames >16.7 ms (of 640) | 639 | 639 |
| Frames >33.3 ms | 639 | 639 |
| Longest main-thread stall | 142.80 ms | 118.10 ms |

| Process/memory metric | Cold | Warm |
|---|---:|---:|
| Idle GPUI UI/runtime | 101.9 MiB | 102.0 MiB |
| Loaded GPUI UI/runtime | 310.8 MiB | 305.6 MiB |
| Loaded UI/runtime processes | 2 | 2 |
| Loaded child processes, including LSP | 4 | 4 |
| typescript-language-server | 140.9 MiB | 140.3 MiB |
| tsserver (combined) | 613.9 MiB | 602.2 MiB |

The stripped release binary is 16,210,848 bytes and its binary-only `.tar.gz`
is 6,695,671 bytes. System Wayland/X11/Vulkan libraries are excluded.

GPUI cannot present or receive frame callbacks directly in this host's Xvfb
session. The accepted harness therefore runs GPUI on software Vulkan inside a
nested Weston software compositor, itself hosted by Xvfb. This makes the
startup and memory measurements useful, but it adds a presentation path that
the web candidates do not share. The slow GPUI scroll and presentation values
are valid for this reproducible headless harness; they are not evidence that
GPUI is slower on an accelerated physical desktop. A physical-GPU repeat is
required before making a cross-runtime smoothness claim.

Raw events and the accepted screenshot are committed beside this checkpoint.
