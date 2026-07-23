import { readFile } from "node:fs/promises";

const source = process.argv[2] ?? new URL("../results/summary.json", import.meta.url);
const input = source === "-"
  ? await new Promise((resolve, reject) => {
      let data = "";
      process.stdin.setEncoding("utf8");
      process.stdin.on("data", (chunk) => { data += chunk; });
      process.stdin.on("end", () => resolve(data));
      process.stdin.on("error", reject);
    })
  : await readFile(source, "utf8");
const summary = JSON.parse(input);
const median = (values) => {
  const sorted = values.filter(Number.isFinite).sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
};
const groups = Object.groupBy(summary.runs, (run) => `${run.candidate}:${run.phase}`);
const output = {};

for (const [name, runs] of Object.entries(groups)) {
  const timingKeys = Object.keys(runs[0].timingsMs);
  const timingsMs = Object.fromEntries(timingKeys.map((key) => [key, median(runs.map((run) => run.timingsMs[key]))]));
  const frameKeys = Object.keys(runs[0].frames);
  const frames = Object.fromEntries(frameKeys.map((key) => [key, median(runs.map((run) => run.frames[key]))]));
  const memory = {};
  for (const label of ["idle", "loaded", "complete"]) {
    const samples = runs.map((run) => run.memory.find((sample) => sample.label === label)).filter(Boolean);
    if (!samples.length) continue;
    const groupNames = [...new Set(samples.flatMap((sample) => sample.processes.map((process) => process.group)))];
    memory[label] = {
      totalMiB: median(samples.map((sample) => sample.processes.reduce((total, process) => total + process.rssKb, 0) / 1024)),
      processCount: median(samples.map((sample) => sample.processes.length)),
      childProcessCount: median(samples.map((sample) => Math.max(0, sample.processes.length - 1))),
      groups: Object.fromEntries(groupNames.map((group) => [group, {
        rssMiB: median(samples.map((sample) => sample.processes.filter((process) => process.group === group).reduce((total, process) => total + process.rssKb, 0) / 1024)),
        processCount: median(samples.map((sample) => sample.processes.filter((process) => process.group === group).length))
      }]))
    };
  }
  output[name] = {
    runs: runs.length,
    timingsMs,
    frames,
    longestMainThreadStallMs: median(runs.map((run) => run.longestMainThreadStallMs)),
    memory
  };
}

console.log(JSON.stringify(output, null, 2));
