# Stackhour

Stackhour is a dependency-free, self-hosted coding time tracker for humans and
coding agents. It replaces WakaTime with one application and a SQLite database
you control.

- `stackhour serve` runs the ingest API, SQLite storage, and web dashboard.
- `stackhour agent` watches local activity and sends heartbeats to the server.
- One agent can run on the server and additional agents can run on Linux or
  macOS machines.
- Offline agents queue heartbeats on disk and retry automatically.

## Building

Stackhour is one Rust binary. Requirements: a stable Rust toolchain (built and
tested on 1.97), Git, and Linux or macOS. SQLite is vendored and there is no
interpreter or package manager to install.

```sh
cargo build --release          # -> target/release/stackhour
./target/release/stackhour doctor
```

Every command in this README is written as `./target/release/stackhour …`.
Copy or symlink the binary onto your `PATH` if you would rather type
`stackhour`; `install` and `bridge install` both deploy whichever binary you
invoked them with, so installing from a stable location is worth doing.

> Stackhour was originally a Node application and was ported to Rust
> behaviour-for-behaviour, including JavaScript's number and rounding
> semantics. The Node implementation, its launcher, and its test suite have
> been removed. The shipped services, including the Telegram bridge and its
> remote workers, do not need a Node runtime. Frontend development uses Node
> and pnpm as build tools. If you are upgrading a deployment that predates this
> change, see
> [docs/bridge-migration.md](docs/bridge-migration.md).

## Quick start

Stackhour is intended to live on a private network. The examples below use a
Tailscale or LAN hostname named `stackhour-server`; replace it with the URL that
your other machines can actually reach. The dashboard is not authenticated, so
do not expose port 4040 directly to the public internet.

### 1. Install the Linux server

```sh
git clone https://github.com/NikitaVoitik/stackhour.git ~/stackhour
cd ~/stackhour
cargo build --release
mkdir -p "$HOME/dev"

./target/release/stackhour init server \
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
./target/release/stackhour doctor
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
./target/release/stackhour install server
```

### 2. Enroll another machine

On the server, create a credential for the exact machine name you want to see
in reports:

```sh
cd ~/stackhour
./target/release/stackhour token create nikita-macbook
```

The command prints one copy-and-paste command containing an enrollment code.
The code packages the server URL, machine name, and token. It is encoded, not
encrypted: treat it as a password and do not post it in chat or commit it.

On the MacBook:

```sh
git clone https://github.com/NikitaVoitik/stackhour.git ~/stackhour
cd ~/stackhour
cargo build --release

# Paste the generated command and add roots before --install, for example:
./target/release/stackhour init agent \
  --enrollment=PASTE_THE_GENERATED_CODE \
  --project-root="$HOME/dev" \
  --project-root="$HOME/client" \
  --install

./target/release/stackhour doctor
```

The macOS installer creates
`~/Library/LaunchAgents/com.stackhour.agent.plist`, loads it with `launchctl`,
and writes logs to `/tmp/stackhour-agent.log`. The first poll can trigger an
Automation permission prompt. For focused-window project detection, also grant
Accessibility permission to your terminal in System Settings → Privacy &
Security.

The same enrollment command works on another Linux machine; `--install` creates
only its `stackhour-agent.service`.

### Rotate or revoke a machine

```sh
# Server: rotate the credential and print a new enrollment command.
./target/release/stackhour token create nikita-macbook --force

# MacBook: apply the replacement and restart/reinstall the launch agent.
./target/release/stackhour init agent --enrollment=NEW_CODE --force --install

# Server: permanently reject that machine's current credential.
./target/release/stackhour token revoke nikita-macbook

# Names only; secrets are never listed.
./target/release/stackhour token list
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
from `crates/stackhour-core/src/pricing.rs`, not invoices or actual
subscription spend.

An illustrative stored heartbeat looks like this:

```json
{
  "time": 1784383200.25,
  "machine": "nikita-macbook",
  "source": "codex-desktop",
  "project": "NikitaVoitik/stackhour",
  "entity": "/Users/nikita/dev/stackhour/crates/stackhour-server/src/lib.rs",
  "entity_type": "file",
  "category": "ai coding",
  "language": "Rust",
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
./target/release/stackhour init server --public-url=http://127.0.0.1:4141 --port=4141
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

## Config registry

A config-directory registry extends the Telegram bridge — custom engines,
agents, skills, commands, and prompt overrides — with no code changes. It
lives beside `config.json`, rooted at the config directory
(`~/.config/stackhour/`, overridable with `STACKHOUR_CONFIG_DIR`):

```
engines/<name>.toml       extra CLI engines; claude and codex ship built in
agents/<name>/agent.toml  + soul.md (+ overlays); supports `extends`
skills/<name>/skill.toml  + skill.md
commands/<name>.toml      one file per verb; the file stem is the verb
prompts/<name>.md         override a shipped template, or define a new one
```

Precedence is built-ins < the `bridge` object in `config.json` < these files <
environment (`STACKHOUR_AGENT`, `STACKHOUR_ENGINE`, `STACKHOUR_TARGET`). An
override replaces the shipped entity of the same name wholesale; only an
agent's `extends` merges. `start`, `help`, `menu`, and `stop` are reserved and
cannot be shadowed. A file that fails to parse or validate is skipped, never
fatal, and the reason is collected for callers to surface. Absent directories
mean built-ins only. See
`crates/stackhour-core/src/registry/defaults/README.md` for the full schema.

The loader, validation, cycle detection, hot reload, engine selection, argv
assembly, soul composition, skill loading, and process spawning are covered by
integration tests, including one that adds a brand-new engine by config alone
and spawns it. The running bridge coordinator and worker both read these
directories, and `stackhour bridge doctor` reports each validation error as its
own `✗ registry: …` line after the standard checks.

## Modules (compile-time and runtime)

Stackhour is four products sharing one binary: a time **tracker**, a local
**agent**, the legacy Telegram **bridge**, and the multi-client **control**
plane. Not every machine wants all four, so
each can be switched off at either of two layers.

| Module | Verbs |
|---|---|
| `tracker` | `serve`, `status`, `token`, `data`, `backup`, `migrate tempo`, `import-wakatime`, `init server`, `install server` |
| `agent` | `agent`, `init agent`, `install agent` |
| `bridge` | `bridge *` |
| `control` | `control hub`, `control node`, `control update` |

`doctor` belongs to no module and is never gated — it is the diagnostic of
last resort. `stackhour-core` (config, paths, tokens, the registries) is the
shared spine and is never optional.

### Runtime: the `modules` block

```json
{ "modules": { "tracker": true, "agent": true, "bridge": false, "control": true } }
```

The gate fails open. An absent `modules` key, an absent sub-key, `null`, or a
malformed block (`3`, `[]`, a string) all mean **everything enabled** — a
config written before this feature existed behaves exactly as it always did.
Only an explicitly present, JS-falsy value disables: `false`, `0`, `""`.

**`"bridge": "false"` ENABLES the bridge.** A non-empty string is truthy in
JavaScript, and this config reads through the same coercion as every other
toggle in it. Write the bare `false` literal, not a quoted one.

### Compile time: build without a module

```sh
cargo build -p stackhour --no-default-features --features bridge   # cheap leader VPS
cargo build -p stackhour --no-default-features --features agent    # worker box
```

A bridge-only build drops 20 crates from the dependency closure (134 → 114),
including `axum`, `rusqlite`, `libsqlite3-sys`, and the bundled SQLite C
amalgamation. **tokio is still linked** — the bridge needs
`reqwest::blocking` for Telegram long-polling, which pulls tokio and hyper.
An agent-only build still links `rusqlite` for the Zed `threads.db` snapshot;
only a bridge-only build is SQLite-free. A plain `cargo build` turns
everything on and is unchanged.

### When a verb refuses

```
stackhour: bridge needs the bridge module, which was not compiled into this binary (rebuild with --features bridge)
stackhour: serve needs the tracker module, which is disabled by "modules.tracker": false in /home/you/.config/stackhour/config.json
```

`not compiled into this binary` means **rebuild**; it never names a config
key, because editing config cannot fix it. `disabled by "modules.X": false`
means **edit that config file**, whose path the message spells out.

Both exit **2** — which `bridge migrate` also uses for "destinations exist",
`bridge return` for a missing job id, and `bridge tg-send` for an unreadable
bridge config. Match on the message, not on the code.

See [docs/modules.md](docs/modules.md) for the full model: the registry API,
resolution order, the feature/crate matrix, and how `install server` handles
a disabled agent module.

## Health and service operations

`stackhour doctor` is read-only. It checks the runtime and SQLite support,
config permissions, project roots, watcher inputs, queue state, server
authentication, database integrity, versions, clock skew, parser silence, and
user services. Its first line is the `runtime` check, reporting
`rust <version>`.

It also appends one `✓ module-<name>` line for each module that is off,
naming either the config key to edit or the Cargo feature to rebuild with —
and the `database` check is absent entirely from a build without the
`tracker` feature, while the `sqlite` check is absent only when neither
`tracker` nor `agent` is compiled in (i.e. a bridge-only build, which links
no SQLite at all). An agent-only worker box still reports `sqlite`. Those
absences are what the `module-*` lines are there to explain.

```sh
./target/release/stackhour doctor
./target/release/stackhour doctor --json
./target/release/stackhour status
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
./target/release/stackhour serve
./target/release/stackhour agent --once
./target/release/stackhour agent
```

## Data management

Inspect the database without changing it:

```sh
./target/release/stackhour data stats
./target/release/stackhour data stats --json
```

Export deterministic, versioned JSONL. Output is mode `0600`, written
atomically, and never replaced unless `--force` is supplied:

```sh
./target/release/stackhour data export \
  --output="$HOME/stackhour-export-2026.jsonl" \
  --from=2026-01-01 \
  --to=2026-12-31
```

Preview retention pruning first. The confirmed operation removes heartbeats
strictly older than the cutoff and imported WakaTime days before its UTC date in
one transaction:

```sh
./target/release/stackhour data prune --before=2025-01-01
./target/release/stackhour data prune --before=2025-01-01 --confirm
```

## Backup and recovery

Create and verify a consistent, standalone SQLite snapshot while the server is
running:

```sh
./target/release/stackhour backup create
./target/release/stackhour backup create --output="$HOME/backups/stackhour.db"
./target/release/stackhour backup verify "$HOME/backups/stackhour.db"
```

Restore is preview-only without `--confirm`. Stop the server for the confirmed
operation; agents can continue running and will queue activity until it returns.

```sh
systemctl --user stop stackhour-server
./target/release/stackhour backup restore "$HOME/backups/stackhour.db"
./target/release/stackhour backup restore "$HOME/backups/stackhour.db" --confirm
systemctl --user start stackhour-server
./target/release/stackhour doctor
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
./target/release/stackhour token create nikita-macbook --raw
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
WAKATIME_API_KEY=waka_xxx ./target/release/stackhour import-wakatime --days=365
```

Imported rows currently live in `wakatime_days`; they are exportable and
prunable but are not yet merged into dashboard charts.

## Telegram bridge

Stackhour also ships a private Telegram control plane for Claude Code and
Codex: a coordinator on the always-on Linux machine owns the Telegram
connection, and a Mac worker polls it over outbound SSH — no open ports on the
Mac, jobs queue while it sleeps.

```sh
./target/release/stackhour bridge install coordinator   # on the Linux machine
./target/release/stackhour bridge install worker        # on the Mac
./target/release/stackhour bridge doctor coordinator    # end-to-end health check
```

`bridge install` copies the binary it was invoked with into the runtime
directory, and a worker reaches its coordinator by running
`<remoteDir>/stackhour bridge claim` over SSH. Neither side needs an
interpreter installed. A worker enrolled before this change still has a
`remoteNode` key in its `worker-config.json`; it is ignored, and re-running
`bridge install worker` drops it.

See [docs/bridge.md](docs/bridge.md) for the full guide and
[SECURITY.md](SECURITY.md) for the security model.

## Multi-client control plane

The control plane has a durable coordinator, outbound execution nodes, a web
control panel, the persistent Telegram assistant **Claire**, and real Claude
and Codex command-line adapters. Commands for an offline node stay in SQLite
and run after the node reconnects.

```sh
curl -fsSL https://github.com/NikitaVoitik/stackhour/releases/latest/download/install-stackhour.sh |
  sh -s -- control install hub \
  --bind=127.0.0.1:4050 \
  --public-url=https://control.example.com
```

Open the coordinator URL. Enter the client token. Use **Add machine** to install
a node on the coordinator or on an SSH machine. The SSH connection is used only
for setup. Normal node traffic uses an outbound WebSocket connection.

Claire keeps a durable Telegram conversation and can create, follow up on, and
stop Stackhour tasks. Use `/claude` or `/codex` to switch her engine without
starting a new conversation. Her personality, active engine, per-engine model,
reasoning effort, node, workspace, and optional
[OptMem](https://github.com/VictorTaelin/OptMem) executable are managed in the
authenticated **Claire settings** panel.

The same panel contains checksum-verified **Stackhour updates**. Manual update
checks and installation are authenticated; automatic updates are opt-in and
can include all currently connected execution nodes.

See [docs/control-plane.md](docs/control-plane.md) for installation, TLS, node
setup, updates, security, Telegram migration, recovery, and current limits.

[docs/architecture/remote-agent-control-plane.md](docs/architecture/remote-agent-control-plane.md)
is the design: a source-level study of T3 Code, Zed, and Claude Code, the
decision to use ACP as an engine adapter rather than a network protocol, the
`Task → Run → ProviderSession` model, and the remaining implementation phases.

## Updating and testing

```sh
cd ~/stackhour
git pull --ff-only

cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

systemctl --user restart stackhour-server stackhour-agent   # Linux server
```

## Releases

Change `[workspace.package].version` in `Cargo.toml` to publish a release.
After that change reaches `master`, GitHub Actions builds checksum-verified
installers for:

- Linux x86-64 (static musl; smoke-tested on Debian 12 and Amazon Linux 2023)
- Linux ARM64 (static musl; smoke-tested on Debian 12 and Amazon Linux 2023)
- macOS Apple Silicon

Each platform has a full binary and a smaller bridge-only binary. Set
`STACKHOUR_RELEASE_FLAVOR=bridge` when running the release installer on a
coordinator that only needs `stackhour bridge`; the default flavor is `full`.
The workflow creates the matching `v<version>` tag and GitHub release. It does
not create a second release when the version already exists.

To migrate the old Tempo tracker database without losing committed WAL rows:

```sh
stackhour migrate tempo --from=/path/to/tempo.db
```

The command uses SQLite's online backup API, refuses to overwrite the
Stackhour database, migrates and verifies the copied schema and heartbeat
count, and leaves the Tempo source untouched.

The suite uses temporary configs, databases, queues, watcher fixtures, and
ephemeral loopback ports. It never reads or writes the live Stackhour config,
database, or `~/.claude-remote/` bridge queue.

Restarting the services picks up a rebuilt binary only if their units point at
it; `stackhour install` writes units naming the binary that ran it.

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
