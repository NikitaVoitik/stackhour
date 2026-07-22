import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";

const base = join(dirname(fileURLToPath(import.meta.url)), "..");
const command = (name, args = []) => {
  try { return execFileSync(name, args, { encoding: "utf8" }).trim(); }
  catch { return null; }
};
const manifest = {
  capturedAt: new Date().toISOString(),
  hostname: os.hostname(),
  platform: os.platform(),
  release: os.release(),
  arch: os.arch(),
  cpu: os.cpus()[0]?.model,
  logicalCpus: os.cpus().length,
  totalMemoryBytes: os.totalmem(),
  node: process.version,
  rustc: command("rustc", ["--version"]),
  webkitgtk: command("pkg-config", ["--modversion", "webkit2gtk-4.1"]),
  chromium: command("chromium", ["--version"]),
  fixtureCommit: command("git", ["rev-parse", "benchmark/common"]),
  display: { server: "Xvfb", geometry: "1280x800x24", dpi: 96 },
  coldDefinition: "fresh application XDG directory; kernel page cache retained",
  warmDefinition: "reused application XDG directory after priming launch"
};
await mkdir(join(base, "results"), { recursive: true });
await writeFile(join(base, "results", "machine.json"), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(JSON.stringify(manifest, null, 2));
