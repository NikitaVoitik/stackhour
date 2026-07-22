import { readFile, readdir, writeFile } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const base = join(dirname(fileURLToPath(import.meta.url)), "..");
const raw = join(base, "results", "raw");
const required = ["process_start", "first_frame", "project_tree_visible", "project_open_requested", "project_scan_complete", "tree_presented", "file_click", "disk_read_complete", "text_presented", "stable_frame", "lsp_request", "lsp_response", "outline_presented", "frame", "memory_sample"];
const percentile = (values, p) => values.slice().sort((a, b) => a - b)[Math.min(values.length - 1, Math.ceil(values.length * p) - 1)];
const delta = (events, start, end) => {
  const a = events.find((event) => event.event === start)?.timestampNs;
  const b = events.find((event) => event.event === end)?.timestampNs;
  return a && b ? (Number(b) - Number(a)) / 1e6 : null;
};

const files = (await readdir(raw)).filter((name) => name.endsWith(".jsonl"));
const runs = [];
for (const file of files) {
  const events = (await readFile(join(raw, file), "utf8")).trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const names = new Set(events.map((event) => event.event));
  const missing = required.filter((event) => !names.has(event));
  if (missing.length) throw new Error(`${file}: missing ${missing.join(", ")}`);
  const frames = events.filter((event) => event.event === "frame").map((event) => event.durationMs);
  const memory = events.filter((event) => event.event === "memory_sample");
  runs.push({
    file,
    candidate: events[0].candidate,
    phase: events[0].phase,
    timingsMs: {
      launchToFirstFrame: delta(events, "process_start", "first_frame"),
      launchToTree: delta(events, "process_start", "project_tree_visible"),
      projectOpenToTree: delta(events, "project_open_requested", "tree_presented"),
      fileClickToRead: delta(events, "file_click", "disk_read_complete"),
      fileClickToPresented: delta(events, "file_click", "text_presented"),
      fileSwitchToStable: delta(events, "file_click", "stable_frame"),
      lspRoundTrip: delta(events, "lsp_request", "lsp_response"),
      lspResponseToOutline: delta(events, "lsp_response", "outline_presented")
    },
    frames: {
      count: frames.length,
      medianMs: percentile(frames, 0.5),
      p95Ms: percentile(frames, 0.95),
      p99Ms: percentile(frames, 0.99),
      worstMs: Math.max(...frames),
      over16_7: frames.filter((value) => value > 16.7).length,
      over33_3: frames.filter((value) => value > 33.3).length
    },
    longestMainThreadStallMs: Math.max(...events.filter((event) => event.event === "main_thread_stall").map((event) => event.durationMs), 0),
    memory
  });
}
const groups = Object.groupBy(runs, (run) => `${run.candidate}:${run.phase}`);
for (const [group, values] of Object.entries(groups)) {
  if (values.length < 5) throw new Error(`${group}: only ${values.length} valid runs; five required`);
}
await writeFile(join(base, "results", "summary.json"), `${JSON.stringify({ generatedAt: new Date().toISOString(), runs }, null, 2)}\n`);
console.log(`validated ${runs.length} runs across ${Object.keys(groups).length} candidate/phase groups`);
