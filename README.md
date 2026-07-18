# Stackhour

Self-hosted coding time tracker. One Node app, two roles:

- **`stackhour serve`** — runs on the Linux server: ingest API + SQLite + web dashboard.
- **`stackhour agent`** — runs on every machine (Mac + Linux): watches activity and ships heartbeats to the server. Offline-safe (disk queue, retries).

No npm dependencies. Requires Node ≥ 22.

## What the agent tracks, with zero editor plugins

| Signal | Covers | How |
|---|---|---|
| File saves in `projectRoots` | WebStorm, Zed local, **Zed remote** (on the server), any editor, manual edits | mtime scan every tick; git branch from `.git/HEAD` |
| `~/.claude/projects/**/*.jsonl` | Claude Code CLI, SDK sessions, Claude Desktop Cowork | incremental JSONL tail; tokens + cost from usage blocks |
| `~/.codex/sessions/**/rollout-*.jsonl` | Codex CLI, Codex IDE ext, Codex Desktop (local sessions) | incremental JSONL tail; tokens + cost from token_count events |
| Frontmost app + window title + idle (macOS) | Claude Desktop chat, Codex Desktop UI, editor focus + project detection | osascript + ioreg poll |
| `/dev/pts/*` atimes (Linux) | You typing over SSH (vim, shells, agent prompts) | pty idle + foreground-process cwd |
| Zed `threads.db` | Zed agent panel / ACP sessions | SQLite copy + updated_at diff |

**Human vs agent:** every heartbeat carries an `actor`. Your prompts, file
saves, SSH typing, and focused-app time are `human`; everything agents do
(including file saves they cause) is `agent`. Agent streams accrue in parallel
per project; your attention is single-threaded. Costs shown are API-equivalent
estimates (see `src/pricing.js`; override via `pricing` in config).

Optionally, official WakaTime editor plugins can be pointed at this server for
keystroke-level granularity: the server speaks the WakaTime heartbeat protocol at
`/api/v1/users/current/heartbeats(.bulk)`. Set in `~/.wakatime.cfg`:

```ini
[settings]
api_url = http://your-server:4040/api/v1
api_key = <your server token>
```

## Setup

```sh
# both machines
git clone <this repo> ~/stackhour
mkdir -p ~/.config/stackhour && cp ~/stackhour/config.example.json ~/.config/stackhour/config.json
$EDITOR ~/.config/stackhour/config.json   # set token, serverUrl, projectRoots

# Linux server
cp ~/stackhour/deploy/stackhour-server.service ~/.config/systemd/user/
cp ~/stackhour/deploy/stackhour-agent.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now stackhour-server stackhour-agent

# Mac — one-shot installer (writes config, loads launchd agent, triggers the
# Automation permission prompt; grant Accessibility too for window titles)
git clone <this repo> ~/stackhour
~/stackhour/deploy/setup-mac.sh http://your-server:4040 <token> ~/dev ~/client
```

Dashboard: `http://your-server:4040/`. CLI: `stackhour status`.

Run the dependency-free reliability suite:

```sh
node --experimental-sqlite --no-warnings --test test/*.test.mjs
```

The suite uses temporary databases, state, queues, watcher fixtures, and an
ephemeral localhost port; it never reads the live Stackhour config or writes the
live database.

Backfill history from wakatime.com (key in config or `WAKATIME_API_KEY`):

```sh
stackhour import-wakatime --days=365
```

## Tuning

Everything lives in `~/.config/stackhour/config.json` (defaults in `src/config.js`):

- `summary.capSeconds` — max seconds one heartbeat can earn (default 120).
  Raise for more generous totals, lower for stricter ones.
- `agent.intervalSeconds` — tick rate (default 20s).
- `agent.apps` — which macOS apps to track and how to label them.
- `agent.ignoreDirs` / `maxScanDepth` — file-scan noise control.

The credit model is ~40 lines in `src/summarize.js`; the watchers are one small
file each under `src/agent/`. Fork away.

## Notes & limits

- Codex **cloud** tasks and Claude/ChatGPT **web** usage never touch local disk —
  invisible to any local tracker.
- Pure Claude Desktop chat has no local transcript; it's tracked only via the
  macOS frontmost-app watcher (app-level, not per-conversation).
- The transcript formats (`~/.claude`, `~/.codex`) are undocumented and may
  drift; watchers fail soft (skip unparseable lines).
- Keep the server on a trusted network (Tailscale recommended); the dashboard
  has no auth, and ingest is protected only by the shared token.
