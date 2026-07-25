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

One Rust binary, three products sharing it:

| Module | Verbs |
|---|---|
| `tracker` | `serve`, `status`, `token`, `data`, `backup`, `import-wakatime`, `init server`, `install server` |
| `agent` | `agent`, `init agent`, `install agent` |
| `bridge` | `bridge *` (Telegram control plane for Claude Code and Codex) |

Each module can be compiled out (Cargo features) *and* switched off at runtime
(the `modules` block in config.json). `doctor` is never gated. See
`docs/modules.md`.

The project was ported from Node behaviour-for-behaviour, including
JavaScript's number and rounding semantics. **The Node implementation has been
removed** — nothing needs a Node runtime, including bridge workers, which run
`<remoteDir>/stackhour bridge claim` over SSH. Comments referencing
`src/*.js` or `coordinator.mjs` are provenance pointing at git history; they
are accurate and worth keeping.

## Build and test

```sh
cargo build --release                                    # -> target/release/stackhour
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

**There is no CI.** The GitHub Actions workflow was removed; nothing runs these
for you. Before pushing anything that touches module wiring, run the reduced
feature combinations by hand — a build missing a module fails at COMPILE time,
long before any assertion, so `cargo test --workspace` alone will not catch it:

```sh
cargo test -p stackhour --no-default-features --features bridge
cargo test -p stackhour --no-default-features --features tracker
cargo test -p stackhour --no-default-features --features agent
cargo test -p stackhour --no-default-features --features tracker,agent
cargo build -p stackhour --no-default-features
```

The dependency-tree claims in `README.md` and `docs/modules.md` — that a
bridge-only build links no SQLite and no HTTP server, but *does* still link
tokio via `reqwest::blocking` — were also only ever checked by that workflow.
If you change the feature graph, re-check them:

```sh
! cargo tree -p stackhour --no-default-features --features bridge -i libsqlite3-sys
! cargo tree -p stackhour --no-default-features --features bridge -i rusqlite
! cargo tree -p stackhour --no-default-features --features bridge -i axum
  cargo tree -p stackhour --no-default-features --features bridge -i tokio
```

The last one is the honest non-claim and is expected to *succeed*. If it ever
stops finding tokio, update the docs rather than quietly dropping the check.

macOS matters here and no machine covers it automatically any more: the agent's
frontmost-window watcher and its launchd installer only build and run there.

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

`docs/architecture/remote-agent-control-plane.md` is the design for the next
phase: a multi-client control plane (Telegram + web/PWA + desktop) over durable
tasks and approvals, with execution nodes dialling out to a hub and ACP as an
engine adapter rather than a network protocol. It is a recommendation, not yet
built — do not treat its entity names as existing code.
