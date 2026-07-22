import { mkdir, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", ".fixture");
await rm(root, { recursive: true, force: true });
await mkdir(join(root, "src"), { recursive: true });

const selected = [];
for (let line = 1; line <= 20_000; line += 1) {
  if (line % 7 === 1) selected.push(`export interface Item${line} { id: number; label: string }`);
  else if (line % 7 === 2) selected.push(`export const item${line}: Item${line - 1} = { id: ${line}, label: "row-${line}" };`);
  else if (line % 7 === 3) selected.push(`export function format${line}(value: Item${line - 2}): string { return value.label; }`);
  else if (line % 7 === 4) selected.push(`// deterministic benchmark source line ${line}`);
  else if (line % 7 === 5) selected.push(`const enabled${line}: boolean = ${line % 2 === 0};`);
  else if (line % 7 === 6) selected.push(`export type Result${line} = Item${line - 5} | null;`);
  else selected.push(`void item${line - 5};`);
}
await writeFile(join(root, "src", "selected.ts"), `${selected.join("\n")}\n`);
await writeFile(
  join(root, "src", "alternate.ts"),
  `${selected.map((line) => line.replaceAll("row-", "alternate-")).join("\n")}\n`,
);

for (let directory = 0; directory < 80; directory += 1) {
  const folder = join(root, "packages", `module-${String(directory).padStart(2, "0")}`, "src");
  await mkdir(folder, { recursive: true });
  for (let file = 0; file < 64; file += 1) {
    const id = directory * 64 + file;
    const body = [
      `export interface Record${id} {`,
      "  id: number;",
      "  name: string;",
      "}",
      "",
      `export const record${id}: Record${id} = {`,
      `  id: ${id},`,
      `  name: "record-${id}",`,
      "};",
      "",
      `export const label${id} = record${id}.name;`,
      "",
    ].join("\n");
    await writeFile(join(folder, `file-${String(file).padStart(2, "0")}.ts`), body);
  }
}

await writeFile(join(root, "package.json"), '{"name":"controlled-fixture","private":true,"type":"module"}\n');
await writeFile(
  join(root, "tsconfig.json"),
  `${JSON.stringify({ compilerOptions: { strict: true, target: "ES2022", module: "ESNext", skipLibCheck: true }, include: ["src/**/*.ts", "packages/**/*.ts"] }, null, 2)}\n`,
);

console.log(root);
