import { mkdir, rm, stat } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { gpuEnvironment, runGpuChecked } from "./gpu-harness.mjs";

const base = join(dirname(fileURLToPath(import.meta.url)), "..");
const executable = join(base, "artifacts", "StackhourBenchReactElectron-linux-x64", "StackhourBenchReactElectron");
await mkdir(join(base, "results", "raw"), { recursive: true });
await stat(executable);
async function run(phase, index, prime = false) {
  const xdg = join("/tmp", `stackhour-bench-react-electron-${phase}`); if (phase === "cold") await rm(xdg, { recursive: true, force: true });
  await mkdir(xdg, { recursive: true });
  const log = join(base, "results", "raw", `react-electron-${phase}-${index}.jsonl`);
  await runGpuChecked({
    candidate: "react-electron",
    phase,
    index,
    command: executable,
    args: ["--no-sandbox", "--ozone-platform=x11"],
    env: gpuEnvironment({
      BENCH_AUTORUN: "1",
      BENCH_PHASE: phase,
      BENCH_RUN_ID: `${phase}-${index}`,
      BENCH_LOG: prime ? "/tmp/stackhour-prime.jsonl" : log,
      BENCH_FIXTURE: join(base, ".fixture"),
      BENCH_LSP: join(base, "node_modules", ".bin", "typescript-language-server"),
      XDG_CONFIG_HOME: join(xdg, "config"),
      XDG_CACHE_HOME: join(xdg, "cache"),
    }),
  });
}
await run("warm", "prime", true);
for (const phase of ["cold", "warm"]) for (let index = 1; index <= 5; index += 1) await run(phase, index);
