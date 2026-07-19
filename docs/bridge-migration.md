# Cutover runbook: Node coordinator → Rust bridge

How to move the live Telegram bridge from `~/.claude-remote/coordinator.mjs`
(systemd unit `claude-coordinator.service`) to `stackhour bridge coordinator`.

**Nobody has performed this cutover yet.** Every command below was rehearsed
against copies and a local mock Bot API; none of it has run against the real
bot token. Read §5 before you decide to do it — the migration does not, on its
own, produce a bootable bridge, and three of the supporting verbs
(`bridge install`, `bridge doctor`, `bridge status`) are unimplemented.

> ## The one rule that cannot be broken
>
> **The Node coordinator and the Rust coordinator must never run at the same
> time.** Both long-poll `getUpdates` against the same bot token. Telegram
> hands each update to whichever poller asks first and then drops it, so two
> pollers silently steal each other's messages: half your replies vanish, and
> nothing in either log says why. Stop and disable the Node unit *before* you
> start the Rust one, every time, in both directions.

---

## 0. Preconditions

- The release binary is built: `cargo build --release --workspace` →
  `target/release/stackhour`. Copy it somewhere stable (e.g.
  `/usr/local/bin/stackhour`) before writing a unit that points at it.
- You can `sudo`. `claude-coordinator.service` is a **system** unit
  (`/etc/systemd/system/claude-coordinator.service`, `User=nikita`), not a
  user unit — `systemctl --user` will not find it.
- `~/.claude-remote/` is backed up, or you are content that the runbook below
  never writes to it except where explicitly stated.
- The Mac worker is running the Node worker and is untouched by this cutover
  (see §2.1).

---

## 1. The migration

### 1.1 What `bridge migrate` actually converts

`stackhour bridge migrate` reads the legacy `config.json` and writes **two**
homes:

| Destination | What lands there |
|---|---|
| `<config-dir>` (default `~/.config/stackhour`) | `config.json` (merged: adds `bridge.defaultEngine/defaultTarget/defaultAgent`), `secrets.json` (0600), `.gitignore`, `engines/{claude,codex}.toml`, `agents/orwell/{agent.toml,soul.md}`, `prompts/{help,ship}.md`, `commands/*.toml` |
| `<runtime-dir>` | `config.json` (0600): `targets{}` + `maxMediaBytes`, **no secrets** |

It never writes back to the legacy file. `--dry-run` performs zero filesystem
writes — no mkdir, no temp file — and masks every secret, so it is safe to
point at the live config.

### 1.2 Run it — dry first

```sh
BIN=/home/nikita/stackhour/target/release/stackhour

$BIN bridge migrate \
  --from /home/nikita/.claude-remote/config.json \
  --to   /home/nikita/.config/stackhour \
  --runtime-dir /tmp/bridge-migrate-scratch \
  --dry-run
```

`--from` has no default on purpose: auto-discovering the live config invites
an accidental run.

**Note the `--runtime-dir /tmp/...`.** See §1.4 — you almost certainly do *not*
want the migrated runtime `config.json`, because the Rust coordinator reads the
Node's existing `~/.claude-remote/config.json` unchanged.

### 1.3 Verify the output before trusting it

Read the plan for these lines specifically. Against the committed fixture (same
shape as the real config) the plan is 15 files, 0 conflicts, and these warnings:

```
warn  engines/*.toml are FROZEN copies of the built-ins ...
warn  /help /menu /stop are RESERVED and /where /new are kind="builtin" ...
warn  target 'gcp' had no codexBin and relied on coordinator.mjs:244's hardcoded fallback; materialised as ~/.local/bin/codex
warn  target 'blort' has no home in the new layout ... /ship cannot switch to it
warn  'maxMediaBytes' absent in source; writing the legacy default 536870912 explicitly
```

Checks, in order:

1. **Every legacy key reached a destination.** Any `legacy key '<k>' reached no
   destination` line is a setting you are about to lose. Stop and fix it.
2. **All three targets are listed** (`3 targets` on the runtime config.json
   line). `blort` is the easiest to drop and is the `/ship` destination.
3. **Secrets are masked but sized.** `bridge.telegramToken 1111…AAAA (51 chars)`
   — the char count is how you confirm the right token without printing it.
4. Now write it (drop `--dry-run`), then re-read the written tree:

```sh
$BIN bridge migrate --from ... --to ... --runtime-dir ... --verify
```

`--verify` re-reads both sides and reports drift; exit 0 = clean, exit 3 =
discrepancies. **Run it before you hand-edit anything** — once you add the
token to the runtime `config.json` (§1.4) `--verify` will legitimately report
that file as differing forever.

Exit codes: `0` ok · `1` bad usage / unreadable source / failed write ·
`2` destinations exist, nothing written (use `--force` to back up + overwrite)
· `3` `--verify` found drift.

### 1.4 The gap: the migrated runtime config does not boot

The migrated `<runtime-dir>/config.json` contains `targets` and
`maxMediaBytes` and deliberately no secrets. But `load_coordinator_cfg` reads
`token`, `chatId` and `elevenLabsApiKey` from **that** file, and **nothing in
the codebase reads `secrets.json` or `STACKHOUR_TELEGRAM_TOKEN`.** Booting
against a freshly migrated runtime dir therefore fails, verified:

```
[…] coordinator config error: config.json must define token, an integer chatId, and gcp/mac targets.
exit=1
```

There are two ways out. **Take the first.**

**(a) Recommended — keep `~/.claude-remote` as the runtime dir and reuse its
config.json as-is.** The Rust coordinator parses the Node's config file
verbatim: `token`, `chatId`, `defaultTarget`, all three `targets`, and
`elevenLabsApiKey` are exactly the keys it wants. This also keeps `state.json`
(offset, active target, engine, sessions), `jobs/`, `inprogress/`, `results/`,
`media/` and `worker-heartbeat` in place, and — critically — keeps the Mac
worker working, because the worker SSHes to `<remoteDir>/claim.mjs` and
`<remoteDir>/return.mjs`, which are the Node scripts already sitting in
`~/.claude-remote`. Nothing on the Mac needs to change.

So: use the migration only for the **config dir** (registry: agents, prompts,
commands, engines, bridge defaults), throw the scratch runtime config away, and
run the daemon with `--runtime-dir /home/nikita/.claude-remote`.

Rehearsed: against a *copy* of the real `~/.claude-remote` (token and
ElevenLabs key replaced with fakes, `apiRoot` pointed at a local mock), the
release binary booted, registered all ten commands, read the real `state.json`,
and sent `🤖 Claude + Codex bridge online. Active: Claude on ☁️ GCP.`

**(b) Not recommended — a separate runtime dir.** Then you must hand-add
`token`, `chatId` and `elevenLabsApiKey` into that `config.json` (mode 0600),
copy `state.json` across, and re-point the Mac worker's `remoteDir` at the new
directory with `claim.mjs`/`return.mjs` present there. More moving parts, no
benefit.

---

## 2. The switch

### 2.1 Write the Rust unit (once, before the cutover)

`stackhour bridge install` is a `todo!()` — it exits 1 with "`bridge` is not
implemented in the Rust port yet". Write the unit by hand. Model it on the
existing one so nothing else changes:

```ini
# /etc/systemd/system/stackhour-bridge.service
[Unit]
Description=Stackhour Telegram bridge coordinator (Rust)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=nikita
WorkingDirectory=/home/nikita/.claude-remote
ExecStart=/usr/local/bin/stackhour bridge coordinator --runtime-dir /home/nikita/.claude-remote
Restart=always
RestartSec=5
UMask=0077
Environment=HOME=/home/nikita
Environment=PATH=/home/nikita/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=STACKHOUR_CONFIG_DIR=/home/nikita/.config/stackhour

[Install]
WantedBy=multi-user.target
```

`STACKHOUR_CONFIG_DIR` is only needed if you migrated somewhere other than the
default `~/.config/stackhour`. **Do not `enable` it yet** — an enabled unit
starting at boot beside the Node one is exactly the double-poller failure.

```sh
sudo systemctl daemon-reload      # do NOT enable/start yet
```

### 2.2 The cutover itself

Run these back to back, in this order, in one sitting:

```sh
sudo systemctl stop claude-coordinator.service
sudo systemctl disable claude-coordinator.service
systemctl is-active claude-coordinator.service      # must print: inactive
pgrep -af coordinator.mjs                            # must print NOTHING

sudo systemctl start stackhour-bridge.service
systemctl is-active stackhour-bridge.service         # must print: active
```

The `pgrep` line is not decoration: `Restart=always` plus a slow shutdown can
leave the old process alive for a few seconds after `stop` returns. Do not
start the Rust unit until it prints nothing.

Enable the Rust unit for boot **only after** §3 passes:

```sh
sudo systemctl enable stackhour-bridge.service
```

Leaving both disabled between those two steps is the safe state — a reboot
mid-cutover then brings up neither, rather than both.

---

## 3. Verify the new bridge, cheapest check first

Stop at the first failure and roll back (§4).

| # | Check | Command | Expect |
|---|---|---|---|
| 1 | Process alive | `systemctl is-active stackhour-bridge.service` | `active` |
| 2 | No second poller | `pgrep -af 'coordinator.mjs\|bridge coordinator'` | exactly one line, the Rust one |
| 3 | Clean start in the log | `tail -5 ~/.claude-remote/coordinator.log` | `coordinator online — <engine> on <target>`, no `config error`, no `registry:` errors |
| 4 | Telegram registered the commands | in Telegram, type `/` | all ten commands listed |
| 5 | Read-only round trip | send `/where` | active engine, target and session — no engine spawned |
| 6 | Keyboard | send `/menu`, tap a target button | toast + repainted keyboard |
| 7 | Local engine end to end | send `hi, reply with one word` | live status message that edits in place, then a final answer with a footer |
| 8 | Cancellation | send a long prompt, then `/stop` | status edits to cancelled |
| 9 | Media | send a photo with a caption | reply references the image; `ls -l ~/.claude-remote/media` shows a new 0600 file |
| 10 | Voice (if you use it) | send a voice note | transcript, then the reply |
| 11 | Mac lane | `/mac`, then a prompt | `working`/`queued` status, then the Mac's answer. Confirms the worker still claims through `claim.mjs` |
| 12 | Session continuity | ask a follow-up that depends on step 7 | it remembers — `state.json` sessions carried over |
| 13 | Survives restart | `sudo systemctl restart stackhour-bridge.service`, then `/where` | online banner, same active target |

Checks 1–3 cost nothing and catch the config/registry failures. 4–6 cost one
API round trip each. 7 onwards spend real model tokens.

---

## 4. Rollback (under a minute)

Nothing the Rust bridge does is destructive to the Node setup: it reads the
same `config.json`, appends to the same `coordinator.log`, and updates
`state.json` in the same shape. So rollback is just swapping which poller runs.

```sh
sudo systemctl stop stackhour-bridge.service
sudo systemctl disable stackhour-bridge.service       # if you enabled it
pgrep -af 'bridge coordinator'                        # must print NOTHING

sudo systemctl enable --now claude-coordinator.service
systemctl is-active claude-coordinator.service        # active
```

Then send `/where` in Telegram to confirm the Node bridge answers.

Notes:

- **`state.json` is shared and is written by whichever daemon is running.** The
  Rust bridge writes the same four keys (`offset`, `active`, `engine`,
  `sessions`), so the Node coordinator picks up where it left off, including
  the Telegram update offset. If you want a guaranteed-untouched copy, take one
  before the cutover: `cp ~/.claude-remote/state.json ~/state.json.pre-cutover`.
- If you migrated with `--force`, the overwritten files are next to the
  originals as `<name>.bak-<unix-ts>`.
- Nothing in `~/.config/stackhour` affects the Node coordinator, so you can
  leave the migrated registry in place after rolling back.

---

## 5. Parity table — honest

### Proven equivalent

Proven means: exercised against a local mock Bot API, and where the row says
"vs Node", the reference `coordinator.mjs` was booted against the same mock
with the same updates and the two request streams were diffed. No test in this
repo has ever contacted `api.telegram.org`.

| Area | Evidence |
|---|---|
| Command surface: all ten commands + descriptions in `setMyCommands`, `/help` body, inline keyboard layout | `test/parity/command-surface.mjs` — passes, diffed against the real `coordinator.mjs` |
| Live status streaming: one status message edited in place, throttling, dedupe, delete-then-deliver | `test/parity/live-status.mjs` — passes, 12 calls matched against the Node |
| Media + voice: `getFile`/file-API download, size cap, 0600 attachment mode, media dir mode, 7-day prune, ElevenLabs transcription | `test/parity/media-and-voice.mjs` — passes, diffed against the Node |
| Final-message rendering: rich-first then delete-status then HTML fallback, chunking, ASCII table conversion | `rendering_parity_with_the_node.rs`, payload-for-payload against a captured Node fixture |
| Telegram transport: retry ladder, 400/404 terminal, 409, 429 + `retry_after`, `not modified`, long-poll parameters | `transport_telegram.rs` |
| `tg-send`: rich-then-plain, chunking at 4000, `--from`, stdin, exit codes | `transport_tg_send.rs` |
| Mac job protocol: dispatch → the **real** `claim.mjs` → the **real** `return.mjs` → delivery; heartbeat; atomic publish | `mac_worker_wire_compat.rs` — runs the owner's actual Node scripts |
| Config parsing: reads the live `~/.claude-remote/config.json` shape unchanged | `bridge_migrate_fixture.rs` over the committed fixture; plus a rehearsal boot against a copy of the real runtime dir |
| Migration content: every legacy key reaches a destination, all three targets and every per-target field survive, dry-run writes nothing, secrets masked | `bridge_migrate_fixture.rs` |

`cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`
both pass on this tree.

### Unproven

| Area | Why |
|---|---|
| Anything against the real bot token | Deliberately never run. Long-polling the live token would steal the owner's messages. First contact with Telegram happens at cutover. |
| Multi-hour stability: memory, thread leaks, socket exhaustion, reconnect after network loss | Longest observed run is a test-length one. `Restart=always` is the only mitigation. |
| A real Claude/Codex child under load — long outputs, tool-heavy runs, timeouts | Tests spawn scripted stand-ins, not the real agents. |
| The Mac worker daemon (`bridge worker`) as a replacement for the Node worker | The Rust worker compiles and is unit-tested, but the recommended cutover leaves the Mac on the Node worker. Migrating the Mac is a separate exercise. |
| Real ElevenLabs transcription | The endpoint is redirected to a local mock in every test. |
| Behaviour on a corrupt/partial `state.json` written by a crashed Node process | Not modelled. |

### Known to differ

| Difference | Detail |
|---|---|
| **`/ship` loses its destination** | `coordinator.mjs:363` hardcodes `state.active = 'blort'`. The Rust `ship_cfg` seeds the ship target from `defaultTarget` and only overrides it from a `ship.target` runtime key the migration never writes — so after cutover `/ship` parks on `gcp`, not `blort`. The `blort` target itself survives in `targets`. Pinned by a test in `bridge_migrate_fixture.rs`; the migrator warns about it. **Workaround:** add `"ship": { "target": "blort", "engine": "claude" }` to the runtime `config.json`. |
| **Secrets live in the runtime config, not `secrets.json`** | The migration writes `secrets.json`, but nothing reads it (§1.4). It is a dead file today. |
| **`bridge install` / `doctor` / `status` / `restart` are unimplemented** | All exit 1 with "not implemented in the Rust port yet". The unit file is hand-written (§2.1). The success message printed by `bridge migrate` tells you to run `stackhour bridge doctor` — **that instruction is wrong**; the command does not exist yet. |
| **A missing `defaultTarget` falls back to `gcp`** | Deliberate. `coordinator.mjs` does `s.active ||= CONFIG.defaultTarget` with no validation, so an absent key leaves `active` undefined and every later `targets[active]` lookup silently misses. That is a bug in the Node, not a contract. The installer's strict validator still rejects the key. |
| **Targets outside `gcp`/`mac` are second-class** | The registry pins targets to `gcp|mac`. `blort` survives in `targets` and still runs, but nothing in the command surface switches to it. |
| **`engines/*.toml` are frozen at migration time** | Migrating with engines (the default) freezes the built-in argv construction to disk; later upstream argv fixes will not reach them. Use `--no-engines` to keep tracking the built-ins. |
| **`codexBin` becomes explicit** | The Node fell back to a hardcoded `~/.local/bin/codex` (coordinator.mjs:244). The migration materialises that path into each local target instead. Same behaviour, now visible. |
| **The runtime `config.json` gains `maxMediaBytes`** | Written explicitly as the legacy default `536870912` rather than left implicit. |
