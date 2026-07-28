# Stackhour — working notes for Claude

## Breaking changes are fine

**Stackhour has no users yet.** Nothing outside this repository depends on its
CLI surface, JSON output, config keys, or on-disk formats.

So: prefer the clean end state over compatibility.

- Do **not** add deprecation shims, aliases, or fallback paths to preserve an
  old spelling. Rename it and move on.
- Do **not** flag changes as `BREAKING` in commit messages, PR bodies, or code
  comments. It is noise — there is nothing to break.
- Do **not** keep a stale name because "scripts might grep for it." They don't.
- When a design has drifted, fix the design rather than layering compatibility
  over it.

The one real deployment is the author's own bridge install (a coordinator on a
Linux box, a worker on a Mac, runtime dir `~/.claude-remote`). Changes that
would require re-running `bridge install` or hand-editing that box's config are
still fine — just say so plainly in the PR so the migration step is known.
`docs/bridge-migration.md` is where that kind of note belongs.

## What this is

One Rust binary, four products sharing it:

| Module | Verbs |
|---|---|
| `tracker` | `serve`, `status`, `token`, `data`, `backup`, `import-wakatime`, `init server`, `install server` |
| `agent` | `agent`, `init agent`, `install agent` |
| `bridge` | `bridge *` (Telegram control plane for Claude Code and Codex) |
| `control` | `control hub`, `control node` (durable multi-client control plane) |

Each module can be compiled out (Cargo features) *and* switched off at runtime
(the `modules` block in config.json). `doctor` is never gated. See
`docs/modules.md`.

The project was ported from Node behaviour-for-behaviour, including
JavaScript's number and rounding semantics. **The Node implementation has been
removed**. Shipped services do not need a Node runtime. Frontend development
uses Node and pnpm as build tools. Bridge workers run
`<remoteDir>/stackhour bridge claim` over SSH. Comments referencing `src/*.js`
or `coordinator.mjs` are provenance pointing at git history; they are accurate
and worth keeping.

The future control-panel shell is in `frontend/`. Its provisional stack is
Tauri 2, React, and strict TypeScript. The current benchmark does not compare
Tauri: it contains incomplete Electron experiments only. Do not state that the
benchmark selected a desktop stack.

## Verification

Select and run one repository verification profile for every change:

```sh
dev/verify-fast       # documentation, comments, and formatting
dev/verify            # normal isolated code changes
dev/verify-full       # cross-service and operational changes
dev/verify-deep       # security boundaries and deep audits
dev/verify-release    # release preparation
```

`AGENTS.md` defines the minimum profile for each affected area. Record the
selected profile and reason in `PLAN.md` before the change. Before reporting
completion, state the selected profile, commands, result, and omitted checks.
Do not silently skip a check because a local tool is missing.

GitHub CI runs only the fast and standard profiles. Full, deep, and release
verification run in the agent loop. The full profile owns the feature matrix,
dependency-tree assertions, dependency policy, installer tests, workflow
analysis, ShellCheck, and the minimum Rust version. The deep profile adds
coverage, Miri, protocol fuzzing, and mutation tests.

Frontend checks use the same levels. Fast runs Prettier and TypeScript.
Standard adds zero-warning ESLint and Vitest. Full adds 100% coverage for the
initial shell, a production build, Knip, dependency-cruiser, size limits, and a
high-severity package audit. Tauri capabilities and permissions require Deep.

macOS matters here. CI runs the workspace suite on an Apple Silicon macOS
runner. The release workflow builds Apple Silicon binaries only.

### Known environment failure

`watch_files::an_unreadable_directory_is_skipped_not_fatal` fails when tests
run as uid 0 (root bypasses the `chmod 000` the test depends on). It is not a
regression; it fails identically on a clean checkout in any root container.

## Things that are load-bearing

- **The heartbeat identity tuple** is `(time, machine, source, project, entity,
  actor)`. Do not drop `actor` or merge human and agent streams when changing
  the schema or summary logic — the split is the product.
- **`tests-fixtures/render-parity/golden.json` is a frozen capture** and cannot
  be regenerated; its generator was a Node script that no longer exists. If the
  parity test fails, the renderer changed. Treat the golden as the spec, and
  only edit it alongside a deliberate rendering change (keeping `cases.json`
  the same length).
- **Two Telegram pollers on one bot token silently steal each other's
  messages.** Never start a second coordinator against a live token.
- **The bridge worker's media path prefix check is a security boundary**, not a
  convenience: `media.path` arrives from the coordinator and is fed to `scp`.
- **Registry/config parse failures are skipped, never fatal**, and the reason is
  collected for callers to surface.

## Documentation must not drift

The README twice claimed features were unimplemented long after they shipped
and had tests asserting the opposite — the bridge, and the config registry. It
also documented an install bug that had already been fixed. When changing
behaviour, grep the README and `docs/` for claims about it and correct them in
the same commit. Distinguish `implemented`, `tested`, `exposed`, and
`operationally proven`; do not conflate them.

## Where this is heading

`docs/architecture/remote-agent-control-plane.md` is the staged design for the
multi-client control plane. The first slice now exists in `stackhour-domain`,
`stackhour-hub`, and `stackhour-node`. It includes durable tasks, runs, events,
outbound nodes, a small web client, Telegram projection, and native Claude and
Codex CLI adapters. Durable engine approvals, ACP, the PWA, Git tools, and
desktop clients remain later phases.
