import { app, BrowserWindow, ipcMain, protocol, net } from "electron";
import { appendFileSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { scan, readSource, documentSymbols } from "./backend.mjs";

const here = dirname(fileURLToPath(import.meta.url));
// Serve the built renderer over a standard-scheme origin so ES module scripts
// load (file:// blocks module scripts at the null origin).
protocol.registerSchemesAsPrivileged([
  { scheme: "app", privileges: { standard: true, secure: true, supportFetchAPI: true } },
]);
const fixture = process.env.BENCH_FIXTURE || join(here, "..", ".fixture");
const logPath = process.env.BENCH_LOG;
const candidate = "svelte-electron"; const phase = process.env.BENCH_PHASE || "visual"; const runId = process.env.BENCH_RUN_ID || "manual";
const emit = (event) => { const row = { schemaVersion: 1, candidate, phase, runId, timestampNs: Number(process.hrtime.bigint()), ...event }; if (logPath) appendFileSync(logPath, `${JSON.stringify(row)}\n`); else console.log(JSON.stringify(row)); };
emit({ event: "process_start" });

function memorySample(label) {
  const rows = [];
  for (const pid of [process.pid, ...app.getAppMetrics().map((metric) => metric.pid)]) {
    try {
      const status = readFileSync(`/proc/${pid}/status`, "utf8"); const cmd = readFileSync(`/proc/${pid}/cmdline`, "utf8").replaceAll("\0", " ");
      const rssKb = Number(status.match(/^VmRSS:\s+(\d+)/m)?.[1] || 0);
      rows.push({ pid, rssKb, group: /typescript-language-server/.test(cmd) ? "typescript-language-server" : /tsserver/.test(cmd) ? "tsserver" : "ui-runtime" });
    } catch {}
  }
  emit({ event: "memory_sample", label, processes: rows });
}
app.whenReady().then(() => {
  const distDir = join(here, "..", "dist");
  protocol.handle("app", (request) => {
    const url = new URL(request.url);
    const rel = decodeURIComponent(url.pathname) === "/" ? "/index.html" : decodeURIComponent(url.pathname);
    return net.fetch(pathToFileURL(join(distDir, rel)).toString());
  });
  const window = new BrowserWindow({ width: 1280, height: 800, useContentSize: true, resizable: false, show: true, backgroundColor: "#17191f", webPreferences: { preload: join(here, "preload.mjs"), contextIsolation: true, sandbox: false, backgroundThrottling: false } });
  window.removeMenu(); window.once("ready-to-show", () => window.show());
  ipcMain.handle("scan", async () => { const files = await scan(fixture); emit({ event: "project_scan_complete", count: files.length }); return files; });
  ipcMain.handle("read", async (_event, path) => { const text = await readSource(fixture, path); emit({ event: "disk_read_complete", path, bytes: Buffer.byteLength(text) }); return text; });
  ipcMain.handle("symbols", async (_event, path) => documentSymbols(fixture, path, process.env.BENCH_LSP || join(here, "..", "node_modules", ".bin", "typescript-language-server")));
  ipcMain.on("telemetry", (_event, row) => { emit(row); if (row.event === "outline_presented") memorySample("loaded"); if (row.event === "benchmark_complete") { memorySample("complete"); setTimeout(() => app.quit(), 50); } });
  ipcMain.on("telemetry-batch", (_event, rows) => rows.forEach(emit));
  window.loadURL("app://bench/index.html");
});
