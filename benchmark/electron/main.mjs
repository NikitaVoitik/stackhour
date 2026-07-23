import { app, BrowserWindow, ipcMain } from "electron";
import { appendFileSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { scan, readSource, documentSymbols, languagePids, stopLanguageServer } from "./backend.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = process.env.BENCH_FIXTURE || join(here, "..", ".fixture");
const logPath = process.env.BENCH_LOG;
const candidate = "react-electron"; const phase = process.env.BENCH_PHASE || "visual"; const runId = process.env.BENCH_RUN_ID || "manual";
const emit = (event) => { const row = { schemaVersion: 1, candidate, phase, runId, timestampNs: Number(process.hrtime.bigint()), ...event }; if (logPath) appendFileSync(logPath, `${JSON.stringify(row)}\n`); else console.log(JSON.stringify(row)); };
emit({ event: "process_start" });

const gpuInfoReady = new Promise((resolve, reject) => {
  const timeout = setTimeout(() => reject(new Error("Electron GPU information timed out")), 10000);
  app.once("gpu-info-update", () => {
    clearTimeout(timeout);
    resolve();
  });
});

async function verifyElectronGpu() {
  await gpuInfoReady;
  const features = app.getGPUFeatureStatus();
  const info = await app.getGPUInfo("basic");
  const compositing = features.gpu_compositing || "unknown";
  const webgl = features.webgl || "unknown";
  const enabled = (value) => /^enabled(?:_|$)/.test(value);
  const accelerated = enabled(compositing) && enabled(webgl);
  emit({
    event: "gpu_renderer",
    accelerated,
    compositing,
    webgl,
    devices: info.gpuDevice || [],
  });
  if (!accelerated || !enabled(compositing) || !enabled(webgl)) {
    throw new Error(
      `Electron hardware rendering unavailable: accelerated=${accelerated}, ` +
      `gpu_compositing=${compositing}, webgl=${webgl}`,
    );
  }
}

function memorySample(label) {
  const rows = [];
  for (const pid of new Set([process.pid, ...app.getAppMetrics().map((metric) => metric.pid), ...languagePids()])) {
    try {
      const status = readFileSync(`/proc/${pid}/status`, "utf8"); const cmd = readFileSync(`/proc/${pid}/cmdline`, "utf8").replaceAll("\0", " ");
      const rssKb = Number(status.match(/^VmRSS:\s+(\d+)/m)?.[1] || 0);
      rows.push({ pid, rssKb, group: /typescript-language-server/.test(cmd) ? "typescript-language-server" : /tsserver/.test(cmd) ? "tsserver" : "ui-runtime" });
    } catch {}
  }
  emit({ event: "memory_sample", label, processes: rows });
}
app.whenReady().then(async () => {
  try {
    await verifyElectronGpu();
  } catch (error) {
    emit({ event: "benchmark_error", message: error.message });
    app.exit(2);
    return;
  }
  const window = new BrowserWindow({ width: 1280, height: 800, useContentSize: true, resizable: false, show: false, backgroundColor: "#17191f", webPreferences: { preload: join(here, "preload.cjs"), contextIsolation: true, sandbox: true } });
  window.removeMenu(); window.once("ready-to-show", () => window.show());
  ipcMain.handle("scan", async () => { const files = await scan(fixture); emit({ event: "project_scan_complete", count: files.length }); return files; });
  ipcMain.handle("read", async (_event, path) => { const text = await readSource(fixture, path); emit({ event: "disk_read_complete", path, bytes: Buffer.byteLength(text) }); return text; });
  ipcMain.handle("symbols", async (_event, path) => documentSymbols(fixture, path, process.env.BENCH_LSP || join(here, "..", "node_modules", ".bin", "typescript-language-server")));
  ipcMain.on("telemetry", (_event, row) => { emit(row); if (row.event === "first_frame") memorySample("idle"); if (row.event === "outline_presented") memorySample("loaded"); if (row.event === "benchmark_complete") { memorySample("complete"); stopLanguageServer(); setTimeout(() => app.quit(), 50); } });
  ipcMain.on("telemetry-batch", (_event, rows) => rows.forEach(emit));
  window.loadFile(join(here, "..", "dist", "index.html"));
});
