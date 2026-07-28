import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const frontend = join(root, "frontend");

async function json(name) {
  return JSON.parse(await readFile(join(frontend, name), "utf8"));
}

const packageJson = await json("package.json");
const tsconfig = await json("tsconfig.json");
const eslint = await readFile(join(frontend, "eslint.config.mjs"), "utf8");
const vitest = await readFile(join(frontend, "vitest.config.ts"), "utf8");

for (const script of [
  "quality:fast",
  "quality:standard",
  "quality:full",
  "format:check",
  "typecheck",
  "lint",
  "test",
  "test:coverage",
  "build",
  "dead-code",
  "dependency-graph",
  "size",
  "audit",
]) {
  assert.equal(typeof packageJson.scripts?.[script], "string", `missing frontend script: ${script}`);
}

assert.match(packageJson.scripts.lint, /--max-warnings 0/);
assert.match(packageJson.scripts.audit, /--audit-level high/);
assert.equal(packageJson.packageManager, "pnpm@11.15.0");

const strictFlags = {
  allowUnreachableCode: false,
  allowUnusedLabels: false,
  exactOptionalPropertyTypes: true,
  forceConsistentCasingInFileNames: true,
  noFallthroughCasesInSwitch: true,
  noImplicitOverride: true,
  noImplicitReturns: true,
  noPropertyAccessFromIndexSignature: true,
  noUncheckedIndexedAccess: true,
  noUncheckedSideEffectImports: true,
  noUnusedLocals: true,
  noUnusedParameters: true,
  skipLibCheck: false,
  strict: true,
  useUnknownInCatchVariables: true,
  verbatimModuleSyntax: true
};

for (const [flag, value] of Object.entries(strictFlags)) {
  assert.equal(tsconfig.compilerOptions?.[flag], value, `tsconfig must set ${flag}=${value}`);
}

assert.match(eslint, /strictTypeChecked/);
assert.match(eslint, /stylisticTypeChecked/);
assert.match(eslint, /no-unnecessary-condition/);
assert.match(eslint, /strict-boolean-expressions/);
assert.match(vitest, /branches: 100/);
assert.match(vitest, /functions: 100/);
assert.match(vitest, /lines: 100/);
assert.match(vitest, /statements: 100/);

console.log("frontend quality contract passed");
