import { invoke } from "@tauri-apps/api/core";

const config = await invoke<{ candidate: string; autorun: boolean }>("config");
let telemetryQueue = Promise.resolve();
const queue = (command: string, payload: Record<string, unknown>) => {
  telemetryQueue = telemetryQueue.then(() => invoke(command, payload).then(() => undefined));
};

window.bench = {
  scan: async () => { await telemetryQueue; return invoke<string[]>("scan_project"); },
  read: async (path) => { await telemetryQueue; return invoke<string>("read_source", { path }); },
  symbols: async (path) => { await telemetryQueue; return invoke<Array<{ name: string; kind: number }>>("document_symbols", { path }); },
  telemetry: (event) => queue("telemetry", { row: event }),
  telemetryBatch: (events) => queue("telemetry_batch", { rows: events }),
  config
};

await import("./mount-vue");
