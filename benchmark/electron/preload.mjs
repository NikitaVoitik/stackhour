import { contextBridge, ipcRenderer } from "electron";

contextBridge.exposeInMainWorld("bench", {
  scan: () => ipcRenderer.invoke("scan"),
  read: (path) => ipcRenderer.invoke("read", path),
  symbols: (path) => ipcRenderer.invoke("symbols", path),
  telemetry: (event) => ipcRenderer.send("telemetry", event),
  telemetryBatch: (events) => ipcRenderer.send("telemetry-batch", events),
  config: { candidate: "vanilla-electron", autorun: process.env.BENCH_AUTORUN === "1" }
});
