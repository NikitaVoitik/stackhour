# Stackhour

Stackhour is a dependency-free, self-hosted coding time tracker for humans and
coding agents. It replaces WakaTime with one Node.js application and a SQLite
database you control.

- `stackhour serve` runs the ingest API, SQLite storage, and web dashboard.
- `stackhour agent` watches local activity and sends heartbeats to the server.
- One agent can run on the server and additional agents can run on Linux or
  macOS machines.
- Offline agents queue heartbeats on disk and retry automatically.

Requirements: Node.js 22 or newer, Git, and Linux or macOS. There are no npm
packages to install. The launcher enables Node's `--experimental-sqlite` flag.

## Quick start

Stackhour is intended to live on a private network. The examples below use a
Tailscale or LAN hostname named `stackhour-server`; replace it with the URL that
your other machines can actually reach. The dashboard is not authenticated, so
do not expose port 4040 directly to the public internet.

### 1. Install the Linux server

```sh
git clone https://github.com/NikitaVoitik/stackhour.git ~/stackhour
cd ~/stackhour
mkdir -p "$HOME/dev"

./bin/stackhour init server \
  --public-url=http://stackhour-server:4040 \
  --project-root="$HOME/dev" \
  --install
```

This one command:

1. creates `~/.config/stackhour/config.json` with mode `0600`;
2. generates a machine-specific token for the server's local agent;
3. stores the public URL used in future enrollment commands;
4. installs and starts `stackhour-server.service` and
   `stackhour-agent.service` as systemd user services.

Check the result:

```sh
./bin/stackhour doctor
curl http://127.0.0.1:4040/api/health
systemctl --user status stackhour-server stackhour-agent
```

Open `http://stackhour-server:4040/` in a browser. If the services must keep
running after logout, enable user lingering once:

```sh
loginctl enable-linger "$USER"
```

If service installation failed after the config was created, it can be retried
without regenerating credentials:

```sh
./bin/stackhour install server
```

### 2. Enroll another machine

On the server, create a credential for the exact machine name you want to see
in reports:

```sh
cd ~/stackhour
./bin/stackhour token create nikita-macbook
```

The command prints one copy-and-paste command containing an enrollment code.
The code packages the server URL, machine name, and token. It is encoded, not
encrypted: treat it as a password and do not post it in chat or commit it.

On the MacBook:

```sh
git clone https://github.com/NikitaVoitik/stackhour.git ~/stackhour
cd ~/stackhour

# Paste the generated command and add roots before --install, for example:
./bin/stackhour init agent \
  --enrollment=PASTE_THE_GENERATED_CODE \
  --project-root="$HOME/dev" \
  --project-root="$HOME/client" \
  --install

./bin/stackhour doctor
```

The macOS installer creates
`~/Library/LaunchAgents/com.stackhour.agent.plist`, loads it with `launchctl`,
and writes logs to `/tmp/stackhour-agent.log`. The first poll can trigger an
Automation permission prompt. For focused-window project detection, also grant
Accessibility permission to the Node executable or terminal in System Settings
→ Privacy & Security.

The same enrollment command works on another Linux machine; `--install` creates
only its `stackhour-agent.service`.

### Rotate or revoke a machine

```sh
# Server: rotate the credential and print a new enrollment command.
./bin/stackhour token create nikita-macbook --force

# MacBook: apply the replacement and restart/reinstall the launch agent.
./bin/stackhour init agent --enrollment=NEW_CODE --force --install

# Server: permanently reject that machine's current credential.
./bin/stackhour token revoke nikita-macbook

# Names only; secrets are never listed.
./bin/stackhour token list
```

## What Stackhour tracks

| Signal | Source label | Actor | Input |
|---|---|---|---|
| File saves under `projectRoots` | `editor-files` | human initially; reattributed when an agent made the edit | recursive mtime scan |
| Claude Code, SDK, and Cowork sessions | `claude-code` / `claude-desktop` | human prompts and agent work separately | `~/.claude/projects/**/*.jsonl` |
| Codex CLI, IDE, and desktop sessions | `codex-*` | human prompts and agent work separately | `~/.codex/sessions/**/rollout-*.jsonl` |
| Active SSH terminals on Linux | `ssh` | human | `/dev/pts/*` activity and foreground cwd |
| Focused applications on macOS | configured app source | human | frontmost window and idle time |
| Zed agent threads | `zed-agent` | agent | Zed `threads.db` updates |
| WakaTime-compatible editor plugins | editor name | human or agent from category | HTTP heartbeat API |

Every heartbeat has an `actor`:

- `human`: prompts, manual file saves, SSH typing, and focused-app activity;
- `agent`: Claude, Codex, Zed agents, tool calls, and matching file saves caused
  by those agents.

Human credit streams are split by machine and source, so switching projects
does not double-count attention. Agent streams additionally split by project,
allowing genuinely parallel agents to accrue time independently. Each
heartbeat earns the gap until the next heartbeat in its stream, capped by
`summary.capSeconds`.

Token and cost fields ride on heartbeats. Costs are API-equivalent estimates
from `src/pricing.js`, not invoices or actual subscription spend.

An illustrative stored heartbeat looks like this:

```json
{
  "time": 1784383200.25,
  "machine": "nikita-macbook",
  "source": "codex-desktop",
  "project": "NikitaVoitik/stackhour",
  "entity": "/Users/nikita/dev/stackhour/src/server.js",
  "entity_type": "file",
  "category": "ai coding",
  "language": "JavaScript",
  "branch": "main",
  "is_write": 1,
  "actor": "agent",
  "tokens_in": 1832,
  "tokens_out": 211,
  "cost": 0.0041
}
```

The unique identity is `(time, machine, source, project, entity, actor)`. Do
not remove `actor` or merge human and agent streams when changing the schema or
summary logic.

## Project identity

Stackhour finds a repository from the heartbeat's cwd or file, reads its Git
remote—including worktree `commondir` metadata—and uses `owner/repository` as
the canonical project. This keeps differently named clones on multiple machines
together and avoids collisions between repositories sharing a basename.

Use aliases when a repository has several remotes or should have a friendlier
name:

```json
{
  "agent": {
    "projectAliases": {
      "github.com/NikitaVoitik/stackhour": "stackhour",
      "/Users/nikita/client/acme-api": "acme/api",
      "legacy-title-from-an-editor": "legacy/app"
    }
  }
}
```

Alias keys are case-insensitive and can be absolute repository paths,
normalized Git remotes, `owner/repository`, or detected labels.

## Configuration and files

Default locations:

| Purpose | Path |
|---|---|
| Configuration | `~/.config/stackhour/config.json` |
| Server database | `~/.local/share/stackhour/stackhour.db` |
| Agent offsets and health | `~/.local/share/stackhour/agent-state.json` |
| Offline queue | `~/.local/share/stackhour/queue.jsonl` |
| Default backups | `~/.local/share/stackhour/backups/` |

Tests and temporary deployments can isolate everything with
`STACKHOUR_CONFIG` and `STACKHOUR_DATA`:

```sh
STACKHOUR_CONFIG=/tmp/stackhour/config.json \
STACKHOUR_DATA=/tmp/stackhour/data \
./bin/stackhour init server --public-url=http://127.0.0.1:4141 --port=4141
```

A representative configuration is:

```json
{
  "server": {
    "host": "0.0.0.0",
    "port": 4040,
    "publicUrl": "http://stackhour-server:4040",
    "db": "/home/nikita/.local/share/stackhour/stackhour.db",
    "tokens": {
      "stackhour-server": "generated-secret",
      "nikita-macbook": "different-generated-secret"
    }
  },
  "agent": {
    "serverUrl": "http://127.0.0.1:4040",
    "token": "generated-secret",
    "machine": "stackhour-server",
    "intervalSeconds": 20,
    "projectRoots": ["/home/nikita/dev"],
    "projectAliases": {},
    "watch": {
      "files": true,
      "claude": true,
      "codex": true,
      "macApps": true,
      "ssh": true,
      "zed": true
    }
  },
  "summary": {
    "capSeconds": 120,
    "lastEventCreditSeconds": 60,
    "reattributeWindowSeconds": 120,
    "joinGapSeconds": 300
  }
}
```

Important tuning fields:

- `agent.intervalSeconds`: watcher polling interval, default 20 seconds;
- `agent.ignoreDirs` and `agent.maxScanDepth`: file scanner limits;
- `agent.apps`: macOS process-to-source mappings and title patterns;
- `summary.capSeconds`: maximum credit from a heartbeat, default 120 seconds;
- `summary.reattributeWindowSeconds`: file-save-to-agent-edit matching window;
- `pricing`: per-model API pricing overrides in USD per million tokens.

## Health and service operations

`stackhour doctor` is read-only. It checks Node and SQLite support, config
permissions, project roots, watcher inputs, queue state, server authentication,
database integrity, versions, clock skew, parser silence, and user services.

```sh
./bin/stackhour doctor
./bin/stackhour doctor --json
./bin/stackhour status
```

Linux service operations:

```sh
systemctl --user status stackhour-server stackhour-agent
journalctl --user -u stackhour-server -u stackhour-agent -f
systemctl --user restart stackhour-server stackhour-agent
```

macOS agent operations:

```sh
launchctl print "gui/$(id -u)/com.stackhour.agent"
tail -f /tmp/stackhour-agent.log
launchctl kickstart -k "gui/$(id -u)/com.stackhour.agent"
```

Manual foreground mode, useful for debugging or containers:

```sh
./bin/stackhour serve
./bin/stackhour agent --once
./bin/stackhour agent
```

## Data management

Inspect the database without changing it:

```sh
./bin/stackhour data stats
./bin/stackhour data stats --json
```

Export deterministic, versioned JSONL. Output is mode `0600`, written
atomically, and never replaced unless `--force` is supplied:

```sh
./bin/stackhour data export \
  --output="$HOME/stackhour-export-2026.jsonl" \
  --from=2026-01-01 \
  --to=2026-12-31
```

Preview retention pruning first. The confirmed operation removes heartbeats
strictly older than the cutoff and imported WakaTime days before its UTC date in
one transaction:

```sh
./bin/stackhour data prune --before=2025-01-01
./bin/stackhour data prune --before=2025-01-01 --confirm
```

## Backup and recovery

Create and verify a consistent, standalone SQLite snapshot while the server is
running:

```sh
./bin/stackhour backup create
./bin/stackhour backup create --output="$HOME/backups/stackhour.db"
./bin/stackhour backup verify "$HOME/backups/stackhour.db"
```

Restore is preview-only without `--confirm`. Stop the server for the confirmed
operation; agents can continue running and will queue activity until it returns.

```sh
systemctl --user stop stackhour-server
./bin/stackhour backup restore "$HOME/backups/stackhour.db"
./bin/stackhour backup restore "$HOME/backups/stackhour.db" --confirm
systemctl --user start stackhour-server
./bin/stackhour doctor
```

The input and replacement are integrity-checked. The previous database is kept
beside the live DB as `stackhour.db.pre-restore-<timestamp>`.

Configuration and tokens are intentionally not included in database backups.
Back up `~/.config/stackhour/config.json` separately with appropriate secret
handling.

## WakaTime plugin compatibility

The server accepts official WakaTime heartbeat routes. Create a raw token for
the editor's machine, then configure its plugin:

```sh
./bin/stackhour token create nikita-macbook --raw
```

```ini
[settings]
api_url = http://stackhour-server:4040/api/v1
api_key = PASTE_THE_RAW_TOKEN
```

Machine-scoped authentication requires the plugin's `X-Machine-Name` header to
match the enrolled name. Stackhour classifies ordinary editor categories as
human and AI categories as agent work.

Historical WakaTime summaries can be imported with an API key in
`wakatime.apiKey` or `WAKATIME_API_KEY`:

```sh
WAKATIME_API_KEY=waka_xxx ./bin/stackhour import-wakatime --days=365
```

Imported rows currently live in `wakatime_days`; they are exportable and
prunable but are not yet merged into dashboard charts.

## Updating and testing

```sh
cd ~/stackhour
git pull --ff-only
node --experimental-sqlite --no-warnings --test test/*.test.mjs
systemctl --user restart stackhour-server stackhour-agent   # Linux server
```

The test suite uses temporary configs, databases, queues, watcher fixtures, and
ephemeral loopback ports. It never reads or writes the live Stackhour config or
database.

## Limits

- Codex cloud tasks and Claude/ChatGPT web sessions do not write local watcher
  data and are invisible.
- Pure Claude Desktop chat has no local transcript and is tracked only through
  macOS focused-app activity.
- Claude, Codex, and Zed storage formats are undocumented. Watchers skip
  malformed records, report errors and likely schema drift, and retain their
  offline queue.
- Zed thread rows currently lack reliable project attribution and are reported
  as `zed-agent`.
- The dashboard and read APIs are unauthenticated. Keep the server behind
  Tailscale, a VPN, or an authenticated reverse proxy.
