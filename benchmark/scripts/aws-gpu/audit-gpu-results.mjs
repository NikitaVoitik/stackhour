import fs from "node:fs";
import path from "node:path";

const root = path.resolve(process.argv[2]);
const outputDir = path.resolve(process.argv[3]);
const candidates = [
  "react-electron",
  "react-tauri",
  "solid-tauri",
  "vue-tauri",
  "gpui",
  "egui",
  "iced",
  "qt-qml",
  "dioxus-desktop",
  "wails",
];

const median = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
};

const round = (value) => Math.round(value * 100) / 100;
const csvEscape = (value) => `"${String(value).replaceAll('"', '""')}"`;
const pmonField = (sample, index) => sample.trim().split(/\s+/)[index];
const maxPmonField = (records, index) => {
  const values = records
    .flatMap((gpu) =>
      gpu.gpuProcessSamples.map((sample) => Number(pmonField(sample, index))),
    )
    .filter(Number.isFinite);
  return values.length ? Math.max(...values) : null;
};
const report = [];

for (const candidate of candidates) {
  const candidateDir = path.join(root, candidate);
  const summary = JSON.parse(
    fs.readFileSync(path.join(candidateDir, "summary.json"), "utf8"),
  );
  const rawFiles = fs
    .readdirSync(path.join(candidateDir, "raw"))
    .filter((name) => name.endsWith(".jsonl"));
  const gpuFiles = fs
    .readdirSync(path.join(candidateDir, "gpu"))
    .filter((name) => name.endsWith(".json"));
  const gpuRecords = gpuFiles.map((name) =>
    JSON.parse(fs.readFileSync(path.join(candidateDir, "gpu", name), "utf8")),
  );
  const cold = summary.runs.filter((run) => run.phase === "cold");
  const warm = summary.runs.filter((run) => run.phase === "warm");
  const loadedRssMb = (runs) =>
    median(
      runs.map((run) => {
        const sample = run.memory.find((item) => item.label === "loaded");
        return (
          sample.processes.reduce((total, process) => total + process.rssKb, 0) /
          1024
        );
      }),
    );

  const record = {
    candidate,
    commit: fs.readFileSync(path.join(candidateDir, "commit.txt"), "utf8").trim(),
    runCount: summary.runs.length,
    coldRunCount: cold.length,
    warmRunCount: warm.length,
    rawFileCount: rawFiles.length,
    gpuEvidenceFileCount: gpuFiles.length,
    gpuEvidenceWithProcessSamples: gpuRecords.filter(
      (gpu) => gpu.gpuProcessSamples.length > 0,
    ).length,
    gpuEvidenceWithFramebufferMb: gpuRecords.filter((gpu) =>
      gpu.gpuProcessSamples.some((sample) => Number(pmonField(sample, 11)) > 0),
    ).length,
    maxObservedAppFramebufferMb: maxPmonField(gpuRecords, 11),
    maxObservedAppSmPercent: maxPmonField(gpuRecords, 5),
    allDirectRendered: gpuRecords.every(
      (gpu) => gpu.gpu.directRendering === "Yes",
    ),
    allTeslaT4: gpuRecords.every(
      (gpu) =>
        gpu.gpu.vendor === "NVIDIA Corporation" &&
        gpu.gpu.renderer.includes("Tesla T4") &&
        gpu.gpu.nvidia.some((line) => line.includes("Tesla T4")),
    ),
    screenshot: fs
      .readdirSync(candidateDir)
      .find((name) => name.endsWith(".png")),
    coldLaunchToTreeMedianMs: round(
      median(cold.map((run) => run.timingsMs.launchToTree)),
    ),
    warmLaunchToTreeMedianMs: round(
      median(warm.map((run) => run.timingsMs.launchToTree)),
    ),
    coldFileSwitchToStableMedianMs: round(
      median(cold.map((run) => run.timingsMs.fileSwitchToStable)),
    ),
    warmFileSwitchToStableMedianMs: round(
      median(warm.map((run) => run.timingsMs.fileSwitchToStable)),
    ),
    coldFrameP95MedianMs: round(
      median(cold.map((run) => run.frames.p95Ms)),
    ),
    warmFrameP95MedianMs: round(
      median(warm.map((run) => run.frames.p95Ms)),
    ),
    coldLoadedRssMedianMb: round(loadedRssMb(cold)),
    warmLoadedRssMedianMb: round(loadedRssMb(warm)),
  };

  record.passedAudit =
    record.runCount === 10 &&
    record.coldRunCount === 5 &&
    record.warmRunCount === 5 &&
    record.rawFileCount === 10 &&
    record.gpuEvidenceFileCount === 11 &&
    record.gpuEvidenceWithProcessSamples === 11 &&
    record.gpuEvidenceWithFramebufferMb === 11 &&
    record.allDirectRendered &&
    record.allTeslaT4 &&
    Boolean(record.screenshot);

  report.push(record);
}

fs.writeFileSync(
  path.join(outputDir, "gpu-audit.json"),
  `${JSON.stringify(
    {
      generatedAt: new Date().toISOString(),
      allCandidatesPassed: report.every((item) => item.passedAudit),
      candidates: report,
    },
    null,
    2,
  )}\n`,
);

const columns = [
  "candidate",
  "coldLaunchToTreeMedianMs",
  "warmLaunchToTreeMedianMs",
  "coldFileSwitchToStableMedianMs",
  "warmFileSwitchToStableMedianMs",
  "coldFrameP95MedianMs",
  "warmFrameP95MedianMs",
  "coldLoadedRssMedianMb",
  "warmLoadedRssMedianMb",
];
const csv = [
  columns.map(csvEscape).join(","),
  ...report.map((item) =>
    columns.map((column) => csvEscape(item[column])).join(","),
  ),
].join("\n");
fs.writeFileSync(path.join(outputDir, "comparison.csv"), `${csv}\n`);

console.log(JSON.stringify(report, null, 2));
if (!report.every((item) => item.passedAudit)) process.exitCode = 1;
