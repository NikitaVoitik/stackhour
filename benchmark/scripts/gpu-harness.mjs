import { execFileSync, spawn } from "node:child_process";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const base = join(dirname(fileURLToPath(import.meta.url)), "..");
const softwareRenderer = /llvmpipe|softpipe|swiftshader|software rasterizer|mesa offscreen|lavapipe/i;

function output(command, args, env) {
  try {
    return execFileSync(command, args, {
      encoding: "utf8",
      env,
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  } catch (error) {
    const detail = error.stderr?.toString().trim() || error.message;
    throw new Error(`${command} ${args.join(" ")} failed: ${detail}`);
  }
}

export function gpuEnvironment(extra = {}) {
  const display = process.env.BENCH_DISPLAY || process.env.DISPLAY || ":0";
  return {
    ...process.env,
    DISPLAY: display,
    BENCH_DISPLAY: display,
    BENCH_GPU_REQUIRED: "1",
    ...extra,
  };
}

export function verifyGpu(env = gpuEnvironment()) {
  if (process.platform !== "linux") {
    throw new Error("The canonical GPU benchmark requires Linux with an NVIDIA-backed Xorg display");
  }
  const glx = output("glxinfo", ["-B"], env);
  const direct = glx.match(/^direct rendering:\s*(.+)$/im)?.[1]?.trim();
  const renderer = glx.match(/^OpenGL renderer string:\s*(.+)$/im)?.[1]?.trim();
  const vendor = glx.match(/^OpenGL vendor string:\s*(.+)$/im)?.[1]?.trim();
  if (!/^yes$/i.test(direct || "")) {
    throw new Error(`DISPLAY=${env.DISPLAY} is not direct-rendered (direct rendering: ${direct || "unknown"})`);
  }
  if (!renderer || softwareRenderer.test(renderer)) {
    throw new Error(`DISPLAY=${env.DISPLAY} is using a software renderer: ${renderer || "unknown"}`);
  }
  const nvidia = output(
    "nvidia-smi",
    ["--query-gpu=index,name,uuid,driver_version", "--format=csv,noheader"],
    env,
  ).split("\n").filter(Boolean);
  if (nvidia.length === 0) {
    throw new Error("nvidia-smi reported no NVIDIA GPUs");
  }
  return { display: env.DISPLAY, directRendering: direct, vendor, renderer, nvidia };
}

async function collectProcessTree(rootPid, tracked) {
  tracked.add(rootPid);
  let entries;
  try {
    entries = await readdir("/proc", { withFileTypes: true });
  } catch {
    return;
  }
  const parents = new Map();
  await Promise.all(entries.filter((entry) => entry.isDirectory() && /^\d+$/.test(entry.name)).map(async (entry) => {
    const pid = Number(entry.name);
    try {
      const status = await readFile(`/proc/${pid}/status`, "utf8");
      const parent = Number(status.match(/^PPid:\s+(\d+)/m)?.[1]);
      if (Number.isInteger(parent)) parents.set(pid, parent);
    } catch {}
  }));
  let changed = true;
  while (changed) {
    changed = false;
    for (const [pid, parent] of parents) {
      if (!tracked.has(pid) && tracked.has(parent)) {
        tracked.add(pid);
        changed = true;
      }
    }
  }
}

function matchingPmonLines(text, tracked) {
  return text.split("\n").filter((line) => {
    if (!line.trim() || line.trimStart().startsWith("#")) return false;
    return [...tracked].some((pid) => new RegExp(`(?:^|\\s)${pid}(?:\\s|$)`).test(line));
  });
}

export async function runGpuChecked({
  candidate,
  phase,
  index,
  command,
  args = [],
  env = gpuEnvironment(),
  stdio = "inherit",
}) {
  const gpu = verifyGpu(env);
  const runId = `${phase}-${index}`;
  const gpuDir = join(base, "results", "gpu");
  await mkdir(gpuDir, { recursive: true });

  let pmon = "";
  let pmonError = "";
  const monitor = spawn("nvidia-smi", ["pmon", "-s", "um", "-d", "1", "-o", "DT"], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  monitor.stdout.setEncoding("utf8");
  monitor.stderr.setEncoding("utf8");
  monitor.stdout.on("data", (chunk) => { pmon += chunk; });
  monitor.stderr.on("data", (chunk) => { pmonError += chunk; });
  const monitorExit = new Promise((resolve) => monitor.once("exit", (code, signal) => resolve({ code, signal })));

  const child = spawn(command, args, { stdio, env });
  const tracked = new Set([child.pid]);
  await collectProcessTree(child.pid, tracked);
  const processPoll = setInterval(() => { void collectProcessTree(child.pid, tracked); }, 100);
  const result = await new Promise((resolve) => {
    child.once("error", (error) => resolve({ error }));
    child.once("exit", (code, signal) => resolve({ code, signal }));
  });
  clearInterval(processPoll);
  await collectProcessTree(child.pid, tracked);

  monitor.kill("SIGTERM");
  await Promise.race([
    monitorExit,
    new Promise((resolve) => setTimeout(resolve, 2000)),
  ]);

  const matched = matchingPmonLines(pmon, tracked);
  const report = {
    schemaVersion: 1,
    candidate,
    phase,
    runId,
    capturedAt: new Date().toISOString(),
    gpu,
    trackedPids: [...tracked].sort((left, right) => left - right),
    gpuProcessSamples: matched,
    pmon,
    pmonError,
  };
  await writeFile(join(gpuDir, `${candidate}-${runId}.json`), `${JSON.stringify(report, null, 2)}\n`);

  if (result.error) throw result.error;
  if (result.code !== 0) {
    throw new Error(`${candidate} exited with ${result.code ?? result.signal}`);
  }
  if (process.env.BENCH_GPU_STRICT !== "0" && matched.length === 0) {
    throw new Error(
      `${candidate} never appeared in nvidia-smi pmon; hardware rendering was not proven. ` +
      `Inspect results/gpu/${candidate}-${runId}.json or set BENCH_GPU_STRICT=0 only for diagnosis.`,
    );
  }
}
