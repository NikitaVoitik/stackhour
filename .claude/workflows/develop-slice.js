export const meta = {
  name: 'develop-slice',
  description: 'Develop one Stackhour control-plane slice: implement -> parallel multi-lens review -> fix -> cargo verify/repair loop',
  whenToUse: 'Building a slice of docs/architecture/remote-agent-control-plane.md Phase 1+ against the existing Rust workspace',
  phases: [
    { title: 'Implement', model: 'opus' },
    { title: 'Review', model: 'opus' },
    { title: 'Fix', model: 'opus' },
    { title: 'Verify', model: 'opus' },
  ],
}

// Every agent in every phase runs on Opus 4.8, per the operator's directive.
const MODEL = 'opus'
const REPO = '/home/nikita/stackhour'

// The slice spec is passed as `args`. Shape:
//   { name, crate, goal, spec (markdown), verifyCmds: [..], testExpectations (markdown) }
// The harness may deliver args as a JSON string; tolerate both.
let slice = args
if (typeof slice === 'string') {
  slice = JSON.parse(slice)
}
if (!slice || !slice.name) {
  throw new Error('develop-slice requires an args object with at least {name, crate, goal, spec}')
}

const FINDINGS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['findings'],
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['severity', 'file', 'summary', 'fix'],
        properties: {
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          file: { type: 'string' },
          line: { type: 'integer' },
          summary: { type: 'string' },
          fix: { type: 'string' },
        },
      },
    },
  },
}

const VERIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['passed', 'ranCommands', 'summary'],
  properties: {
    passed: { type: 'boolean' },
    ranCommands: { type: 'array', items: { type: 'string' } },
    failingCommand: { type: 'string' },
    errorExcerpt: { type: 'string' },
    summary: { type: 'string' },
  },
}

const context = `You are developing the Stackhour project — a dependency-free, self-hosted Rust
coding-time tracker that is growing into a multi-client control plane for remotely executed
coding agents. Repo root: ${REPO}.

Read these before writing code:
- ${REPO}/CLAUDE.md  (working rules: breaking changes are fine, no CI, load-bearing invariants)
- ${REPO}/docs/architecture/remote-agent-control-plane.md  (the design you are implementing)
- ${REPO}/Cargo.toml  (workspace deps — reuse workspace versions via \`x.workspace = true\`)
- an existing crate such as crates/stackhour-store (SQLite/rusqlite conventions) and
  crates/stackhour-core (the shared \`Error\`/\`Result\` types you should reuse).

Environment: cargo is installed via rustup but may not be on PATH in a non-login shell.
If \`cargo\` is not found, run \`export PATH="$HOME/.cargo/bin:$PATH"\` first (or invoke
\`~/.cargo/bin/cargo\` directly). Always run cargo from ${REPO}.

Hard rules:
- New standalone crate under crates/ is auto-included by \`members = ["crates/*"]\`; you do NOT
  edit the root Cargo.toml for membership. Only add to [workspace.dependencies] if another
  existing crate must depend on this one (it must not, for this slice).
- Reuse workspace dependencies (rusqlite bundled, serde, serde_json, chrono, uuid, anyhow) via
  \`{ workspace = true }\`. Reuse stackhour_core::{Error, Result} for the crate's error type.
- Edition 2021, rust-version 1.79. No async in this crate. Match the surrounding code's style,
  comment density, and naming.
- This is greenfield: prefer the clean end state. No deprecation shims, no BREAKING labels.`

phase('Implement')
let implReport
if (slice.skipImplement) {
  log(`skipImplement set — reviewing the crate already on disk at ${slice.crate}`)
  implReport = `The "${slice.name}" crate was already implemented on disk at ${REPO}/${slice.crate} and it already builds, tests, and passes clippy. Review the actual files on disk; there is no fresh implementer report to trust.`
} else {
  const implPrompt = `${context}

## Your task: implement the "${slice.name}" slice

Goal: ${slice.goal}

Specification (implement exactly this, no more):
${slice.spec}

Deliver a compiling, well-tested crate/module. Write thorough unit tests using the workspace
\`tempfile\` dev-dependency for on-disk SQLite. Test the invariants named in the spec. Keep the
public API small and documented with /// doc comments in the style of the existing crates.

Do the work now by editing files under ${REPO}. When done, output a concise summary of the
files you created/changed and the public API surface. Your output is a report, not code.`
  implReport = await agent(implPrompt, { label: `impl:${slice.name}`, phase: 'Implement', model: MODEL })
}

phase('Review')
const LENSES = [
  {
    key: 'correctness-idempotency',
    focus: `Correctness of the durable-log invariants: single monotonic hub-assigned sequence with
no gaps or duplicates; command-receipt idempotency (replaying the same command_id yields exactly
one effect and the same result); node-event UUID de-duplication BEFORE sequence assignment;
after_sequence catch-up returning a gapless tail and then live with no gap or overlap; correct
behaviour under a mid-transaction failure (atomicity of receipt + event). Look for race windows,
non-atomic read-modify-write, and off-by-one in the cursor.`,
  },
  {
    key: 'data-model-fidelity',
    focus: `Fidelity to docs/architecture/remote-agent-control-plane.md: the five entities
(Node, Task, Run, Event, Approval) and their fields; the "Durable data model" field list on
commands/events (command_id, event_id, sequence, task_id, run_id?, provider_session_id?, node_id,
protocol_version, occurred_at, hub receipt time); the "Initial event vocabulary" set exactly (no
invented event types, none missing); approvals carrying scope, expiry, decision, actor. Flag any
drift, extra tables the doc says to defer, or ACP/provider types leaking into the durable schema.`,
  },
  {
    key: 'rust-api-quality',
    focus: `Rust API quality and repo-fit: reuse of stackhour_core::{Error, Result} and workspace
deps (no duplicate/pinned versions); idiomatic rusqlite (prepared statements, params!, correct
transactions, no SQL injection via format!); sensible module layout mirroring stackhour-store;
clippy-cleanliness (this repo builds with -D warnings); test quality and coverage of the stated
invariants; doc-comment presence and accuracy.`,
  },
]

const reviews = await parallel(
  LENSES.map((lens) => () =>
    agent(
      `${context}

## Review the "${slice.name}" slice — lens: ${lens.key}

The implementer just built this slice. Its report:
${implReport}

Read the actual code it produced under ${REPO} (do not trust the report — verify against files)
and review ONLY through this lens:

${lens.focus}

Report concrete, actionable findings. Each finding needs a real file, a one-line defect summary,
and a specific fix. Do not report style nits already consistent with the codebase, and do not
invent problems — if the slice is correct on this lens, return an empty findings array.`,
      { label: `review:${lens.key}`, phase: 'Review', model: MODEL, schema: FINDINGS_SCHEMA },
    ),
  ),
)

const findings = reviews
  .filter(Boolean)
  .flatMap((r) => r.findings || [])
  .filter((f) => f.severity !== 'minor' || true) // keep all; fixer triages

log(`Review complete: ${findings.length} finding(s) across ${LENSES.length} lenses`)

phase('Fix')
let fixReport = 'No findings — fix phase skipped.'
if (findings.length) {
  const blockers = findings.filter((f) => f.severity === 'blocker').length
  const majors = findings.filter((f) => f.severity === 'major').length
  fixReport = await agent(
    `${context}

## Apply review fixes to the "${slice.name}" slice

Three reviewers found ${findings.length} issue(s) (${blockers} blocker, ${majors} major). Apply the
fixes that are genuinely correct. For each finding, either fix it or, if you judge it wrong or
out of scope, say why in your summary. Findings (JSON):

${JSON.stringify(findings, null, 2)}

Edit files under ${REPO}. Keep tests passing and add tests for any fixed invariant that lacked
coverage. Output a summary of what you changed and what you deliberately skipped.`,
    { label: `fix:${slice.name}`, phase: 'Fix', model: MODEL },
  )
}

phase('Verify')
const verifyCmds = slice.verifyCmds && slice.verifyCmds.length
  ? slice.verifyCmds
  : [
      `cargo build -p ${slice.name}`,
      `cargo test -p ${slice.name}`,
      `cargo clippy -p ${slice.name} --all-targets -- -D warnings`,
    ]

let verify = null
const MAX_ROUNDS = 3
for (let round = 0; round < MAX_ROUNDS; round++) {
  verify = await agent(
    `${context}

## Verify the "${slice.name}" slice (round ${round + 1}/${MAX_ROUNDS})

Run these commands from ${REPO}, in order, and report the result honestly:

${verifyCmds.map((c) => `    ${c}`).join('\n')}

${slice.testExpectations ? `Expected behaviour:\n${slice.testExpectations}\n` : ''}
Do NOT edit any files in this step — you are only measuring. Report passed=true ONLY if every
command exits 0. If one fails, set failingCommand and paste the most relevant compiler/test error
lines into errorExcerpt (trim noise, keep the actionable part).`,
    { label: `verify:r${round + 1}`, phase: 'Verify', model: MODEL, schema: VERIFY_SCHEMA },
  )

  if (verify && verify.passed) {
    log(`Verify passed on round ${round + 1}`)
    break
  }
  if (round === MAX_ROUNDS - 1) {
    log(`Verify still failing after ${MAX_ROUNDS} rounds`)
    break
  }

  log(`Verify failed (${verify && verify.failingCommand}); dispatching repair round ${round + 1}`)
  await agent(
    `${context}

## Repair the "${slice.name}" slice — verification failed

The verify step ran \`${verify.failingCommand}\` and it failed:

${verify.errorExcerpt || verify.summary}

Fix the root cause by editing files under ${REPO}. Do not paper over failures (no \`#[ignore]\`,
no deleting the failing assertion unless it is genuinely wrong — if so, explain). Then output what
you changed. The next round will re-run the full command set.`,
    { label: `repair:r${round + 1}`, phase: 'Verify', model: MODEL },
  )
}

return {
  slice: slice.name,
  findings: findings.length,
  verifyPassed: !!(verify && verify.passed),
  verifySummary: verify && verify.summary,
  implReport: implReport.slice(0, 800),
}
