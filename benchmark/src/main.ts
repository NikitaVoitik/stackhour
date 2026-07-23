export {};

declare global {
  interface Window {
    go: {
      main: {
        App: {
          Config(): Promise<{ candidate: string; autorun: boolean }>;
          Scan(): Promise<string[]>;
          Read(path: string): Promise<string>;
          Symbols(path: string): Promise<Array<{ name: string; kind: number }>>;
          Telemetry(event: Record<string, unknown>): Promise<void>;
          TelemetryBatch(events: Array<Record<string, unknown>>): Promise<void>;
        };
      };
    };
  }
}

const backend = window.go.main.App;
const config = await backend.Config();
let telemetryQueue = Promise.resolve();
const queue = (call: () => Promise<void>) => {
  telemetryQueue = telemetryQueue.then(call);
};

window.bench = {
  scan: async () => { await telemetryQueue; return backend.Scan(); },
  read: async (path) => { await telemetryQueue; return backend.Read(path); },
  symbols: async (path) => { await telemetryQueue; return backend.Symbols(path); },
  telemetry: (event) => queue(() => backend.Telemetry(event)),
  telemetryBatch: (events) => queue(() => backend.TelemetryBatch(events)),
  config
};

await import("./renderer");
