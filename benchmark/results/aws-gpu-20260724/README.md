# Stackhour AWS GPU benchmark

All ten desktop implementations completed successfully on AWS instance
`i-056c663c78e4cee0d` (`g4dn.xlarge`) using its NVIDIA Tesla T4.

## Validation

- 100 measured runs: 5 cold and 5 warm per implementation
- 110 GPU traces: one warm-up plus 10 measured traces per implementation
- All 110 traces report direct rendering through NVIDIA/Tesla T4
- All 110 traces contain application graphics-process samples with nonzero GPU
  framebuffer memory
- All ten implementations produced populated 1280x800 screenshots
- GPU display: dedicated Xorg `:99`, OpenGL 4.6, NVIDIA driver 595.71.05
- GPUI's X11 harness rejects the run unless the mapped application surface is
  exactly 1280x800
- The EC2 instance was stopped and the temporary SSH ingress rule was removed
  after result retrieval

The machine manifests, raw JSONL event streams, NVIDIA `pmon` captures,
screenshots, per-candidate summaries, and Xorg logs are under `results/`.
`gpu-audit.json` is the machine-readable validation report, and
`comparison.csv` contains normalized median metrics.

## Median comparison

Each value is the median of five runs. Memory is total tracked workload RSS at
the loaded state. For implementations that launch it, this includes
`typescript-language-server`/`tsserver`, so it is not pure GUI-shell memory and
is not directly comparable to Qt, whose capture did not include an equivalent
external LSP workload. GPUI's UI-runtime processes alone used a median 483.92
MiB cold and 483.78 MiB warm.

| Implementation | Cold launch-to-tree (ms) | Warm launch-to-tree (ms) | Cold frame p95 (ms) | Warm frame p95 (ms) | Cold loaded RSS (MiB) | Warm loaded RSS (MiB) |
|---|---:|---:|---:|---:|---:|---:|
| React + Electron | 744.49 | 697.93 | 14.80 | 14.80 | 1394.16 | 1316.59 |
| React + Tauri | 404.72 | 406.99 | 30.00 | 30.00 | 1125.54 | 1131.87 |
| SolidJS + Tauri | 376.84 | 367.32 | 29.00 | 28.00 | 1120.63 | 1125.13 |
| Vue + Tauri | 390.20 | 384.69 | 28.00 | 29.00 | 1123.97 | 1128.25 |
| Rust GPUI | 709.40 | 765.09 | 17.65 | 21.71 | 1179.22 | 1172.91 |
| Rust egui | 259.50 | 247.37 | 6.22 | 6.49 | 1053.42 | 1057.45 |
| Rust iced | 489.29 | 488.41 | 2.10 | 2.08 | 1117.14 | 1124.49 |
| C++ / Qt Quick | 423.31 | 341.39 | 19.96 | 20.22 | 267.98 | 210.14 |
| Rust Dioxus Desktop | 25372.29 | 25369.40 | 33.91 | 33.73 | 1234.34 | 1220.52 |
| Go Wails | 206.77 | 208.27 | 26.00 | 26.00 | 1258.09 | 1267.88 |

The 1 Hz NVIDIA sampler does not always catch nonzero SM utilization for a
lightweight GUI frame. The renderer identity, direct-rendering check,
application graphics-process classification, nonzero framebuffer allocation,
framework rendering diagnostics, and screenshots jointly establish that these
were hardware-rendered GUI runs rather than software/headless fallbacks.
