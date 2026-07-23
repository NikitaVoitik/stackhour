import { spawn } from "node:child_process";
import { mkdir, rm, stat } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const base = join(dirname(fileURLToPath(import.meta.url)), "..");
const executable = join(base, "artifacts", "StackhourBenchSvelteElectron-linux-x64", "StackhourBenchSvelteElectron");
await mkdir(join(base, "results", "raw"), { recursive: true });
await stat(executable);
async function run(phase, index, prime = false) {
  const xdg = join("/tmp", `stackhour-bench-svelte-electron-${phase}`); if (phase === "cold") await rm(xdg, { recursive: true, force: true });
  await mkdir(xdg, { recursive: true });
  const log = join(base, "results", "raw", `svelte-electron-${phase}-${index}.jsonl`);
  await new Promise((resolve, reject) => {
    const child = spawn("xvfb-run", ["-a", "-s", "-screen 0 1280x800x24 -dpi 96", executable, "--no-sandbox", "--disable-gpu"], { stdio: "inherit", env: { ...process.env, BENCH_AUTORUN: "1", BENCH_PHASE: phase, BENCH_RUN_ID: `${phase}-${index}`, BENCH_LOG: prime ? "/tmp/stackhour-prime.jsonl" : log, BENCH_FIXTURE: join(base, ".fixture"), BENCH_LSP: join(base, "node_modules", ".bin", "typescript-language-server"), XDG_CONFIG_HOME: join(xdg, "config"), XDG_CACHE_HOME: join(xdg, "cache") } });
    child.on("error", reject); child.on("exit", (code) => code === 0 ? resolve() : reject(new Error(`exit ${code}`)));
  });
}
await run("warm", "prime", true);
for (const phase of ["cold", "warm"]) for (let index = 1; index <= 5; index += 1) await run(phase, index);
