# Stackhour — working notes for Claude

## Breaking changes are fine

Stackhour has no external users yet. Prefer a clean end state over aliases,
deprecation shims, fallback paths, or compatibility-only code.

## What this is

One Rust binary with three optional modules:

| Module | Commands |
| --- | --- |
| `tracker` | `serve`, `status`, `token`, `data`, `backup`, `migrate tempo`, `import-wakatime`, `init server`, `install server` |
| `agent` | `agent`, `init agent`, `install agent` |
| `control` | `control hub`, `control node`, `control fake-telegram`, `control install`, `control update` |

Each module can be compiled out with Cargo features and disabled at runtime
through `config.json`. `doctor` is never gated. See `docs/modules.md`.

Shipped services are Rust programs and need no Node runtime. Frontend
development uses Node and pnpm as build tools.

## Keep the footprint small

Pursue the smallest practical binary and the fewest practical dependencies.
Every new dependency needs a clear justification. Prefer the standard library,
an existing dependency, or a smaller alternative while preserving security,
functionality, maintainability, and platform support.

## Verification

Select and run one repository verification profile for every change:

```sh
dev/verify-fast
dev/verify
dev/verify-full
dev/verify-deep
dev/verify-release
```

`AGENTS.md` defines the minimum profile. Record the selected profile and reason
in `PLAN.md` before editing. Before reporting completion, state the profile,
commands, results, and omitted checks.

## Things that are load-bearing

- The heartbeat identity tuple is `(time, machine, source, project, entity,
  actor)`. Do not merge human and coding-agent activity.
- Two Telegram pollers using one bot token can take updates from each other.
- Client and node tokens are separate trust boundaries.
- Commands are persisted before delivery to nodes; reconnect must not lose or
  duplicate accepted work.

## Documentation must not drift

When behavior changes, update the README and `docs/` in the same change.
Distinguish implemented, tested, exposed, and operationally proven behavior.

## Direction

`docs/architecture/remote-agent-control-plane.md` is the staged design. The
implemented slice has durable tasks, runs, events, outbound nodes, a web
client, Telegram, and Claude/Codex CLI adapters. Durable engine approvals, ACP,
richer project tools, and desktop clients remain later phases.
