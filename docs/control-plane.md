# Control plane

The control plane has one coordinator and one or more execution machines.

- The coordinator runs `stackhour control hub`.
- Each execution machine runs `stackhour control node`.
- The browser and each node connect to the coordinator.
- The coordinator stores task history in SQLite.
- The node starts Claude or Codex in its local workspace.

A node makes an outbound WebSocket connection. The coordinator does not open
an SSH connection when it runs a task. It uses SSH only during setup.

## Supported host setup

Use a small Linux machine for the coordinator. This is the recommended setup.
The installer uses a systemd user service on Linux and a launchd user service
on Apple Silicon macOS. Intel macOS is not supported.

Install these tools before you start:

- Git
- Rust 1.87 or later
- Claude Code or Codex on each execution machine
- OpenSSH client on the coordinator if you add SSH machines
- curl and tar on each SSH machine

The coordinator needs enough disk space for the binary and the SQLite event
log. A small machine is sufficient for a few users and nodes.

## Install the coordinator

Install the latest release:

```sh
curl -fsSL https://github.com/NikitaVoitik/stackhour/releases/latest/download/install-stackhour.sh |
  sh -s -- control install hub \
  --bind=127.0.0.1:4050 \
  --public-url=https://control.example.com
```

The release installer does these actions:

1. It detects Linux or macOS and the CPU architecture.
2. It downloads the correct release archive.
3. It verifies the SHA-256 checksum.
4. It installs the binary at `~/.local/bin/stackhour`.
5. It creates or updates the Stackhour config.
6. It creates and starts the control hub service.

To build from source instead, clone the repository and run:

```sh
git clone https://github.com/NikitaVoitik/stackhour.git
cd stackhour
./deploy/install-control.sh hub \
  --bind=127.0.0.1:4050 \
  --public-url=https://control.example.com
```

The installer prints a client token and a node token. Save both tokens in a
password manager. The control panel needs the client token. Nodes use the node
token. The panel does not receive the node token.

For a headless Linux coordinator, make sure the user service can start after a
restart:

```sh
sudo loginctl enable-linger "$USER"
```

Check the service:

```sh
systemctl --user status stackhour-control-hub
curl http://127.0.0.1:4050/health
```

The health response is `ok`.

## Add TLS

The built-in hub serves HTTP and WebSocket traffic. It does not terminate TLS.
Keep it on `127.0.0.1` and put Caddy, Nginx, or another reverse proxy in front
of it.

The proxy must support WebSocket upgrades for these routes:

- `/v1/client/connect`
- `/v1/node/connect`

Use HTTPS for the control panel. Nodes must use a `wss://` URL when traffic
crosses an untrusted network.

## Use the control panel

Open the public coordinator URL. Enter the client token. The browser keeps the
token only in memory. It does not save the token after a page reload.

The panel has these main actions:

- View connected and disconnected machines.
- Select a connected machine.
- Start Claude or Codex on that machine.
- Stop the current run.
- Configure Claire's personality, engine, models, reasoning effort, execution
  node, workspace, and optional memory adapter.
- Add the coordinator as an execution machine.
- Add a remote machine through SSH.
- Check for and install a stable Stackhour release.
- Enable opt-in automatic updates and choose whether connected nodes update
  with the coordinator.

### Add the coordinator as a node

Select **Add machine**, then select **This machine**. Set a machine ID and an
optional default workspace. The coordinator runs the node installer and starts
`stackhour-control-node.service`.

The hub and node can use the same config file. The installer merges both
sections and keeps unrelated config keys.

### Add an SSH machine

First, test SSH from the coordinator:

```sh
ssh user@devbox true
```

This command must succeed without a password prompt. It also adds the host key
to `known_hosts`.

In the panel, select **Add machine**, then select **SSH machine**. Enter:

- The SSH host or SSH config alias.
- The SSH user.
- The SSH port.
- An optional absolute identity-file path on the coordinator.
- A stable machine ID.
- An optional default workspace on the remote machine.

The installer requires strict host-key checks. It does not accept a password or
a private key from the browser. It uses the coordinator SSH agent or the
identity file that you name.

The coordinator downloads the release installer through SSH. The installer
detects the remote operating system and CPU type. It downloads and verifies the
correct Stackhour release and installs it at `~/.local/bin/stackhour`. It then
writes the node configuration and starts the node service. The coordinator and
remote machine can use different operating systems and CPU types.

The coordinator sends the node token through standard input. It does not put
the token in a process argument. The remote installer writes the token to the
mode `0600` config file.

## Install a node without the panel

Copy the Stackhour binary to the execution machine. Then run:

```sh
stackhour control install node \
  --hub-url=wss://control.example.com/v1/node/connect \
  --id=devbox \
  --token=NODE_TOKEN \
  --workspace=/home/user/work
```

You can also start SSH setup from the coordinator:

```sh
stackhour control install ssh \
  --host=devbox \
  --user=user \
  --hub-url=wss://control.example.com/v1/node/connect \
  --id=devbox \
  --token=NODE_TOKEN \
  --workspace=/home/user/work
```

Run `stackhour control install --help` for all options. Use `--no-start` to
write the config and service file without starting the service.

## Files

The default files on Linux are:

```text
~/.local/bin/stackhour
~/.config/stackhour/config.json
~/.local/share/stackhour/control.db
~/.config/systemd/user/stackhour-control-hub.service
~/.config/systemd/user/stackhour-control-node.service
```

The config file has mode `0600`. Service files have mode `0644`. Each service
uses the stable binary path and the `PATH` value from installation time.

## Updates

Open **Claire settings → Stackhour updates** to check the latest stable
release, install it immediately, or enable automatic checks every 1–168 hours.
Automatic updates are disabled by default.

The browser can select only the schedule and whether connected nodes are
included. It cannot provide a release URL, executable, or shell command. The
coordinator:

1. reads `VERSION` and `SHA256SUMS` from Stackhour's fixed official release;
2. refuses downgrades and non-semantic versions;
3. downloads the platform's static Linux or Apple Silicon archive;
4. verifies SHA-256 before extraction;
5. atomically replaces `~/.local/bin/stackhour`;
6. restarts nodes and then the coordinator through a separate systemd or
   launchd update job.

Connected nodes receive an authenticated, ten-minute update command and repeat
the official-version and checksum verification themselves. A disconnected node
is not force-updated; it is picked up by a later automatic check or can be
reinstalled from the panel.

The equivalent manual commands are:

```sh
stackhour control update --check
stackhour control update --role=all
```

The release installer remains available:

```sh
curl -fsSL https://github.com/NikitaVoitik/stackhour/releases/latest/download/install-stackhour.sh |
  sh -s -- control install hub \
  --bind=127.0.0.1:4050 \
  --public-url=https://control.example.com
```

The installer keeps existing tokens, bind settings, database path, and public
URL when you omit those options. Use `--node-token` or `--client-token` only
when you intend to replace a token.

To recover a node that cannot connect at its current protocol version, use the
panel install action again. It downloads the latest release for that machine
and restarts the remote node service.

Linux releases are static musl binaries and are artifact-tested in Debian 12
and Amazon Linux 2023 containers on both x86-64 and ARM64 release runners.

## Telegram migration

Set `control.telegram.enabled` to `true` on the hub. Then stop the old bridge
coordinator before you restart the hub. Two pollers with the same bot token can
take updates from each other.

The Telegram client is a persistent assistant named Claire. A normal message
continues Claire's durable task and provider run instead of creating an
unrelated task. Claire sees recent task activity and can use typed actions to:

- Create a worker task on a Stackhour node.
- Send a follow-up message to a known task and run.
- Stop a known run.

Worker tasks are separate from Claire's own conversation. Their completion or
failure is reported back to Telegram.

Claire's Telegram commands are:

- `/claude` and `/codex` switch the engine. The next message starts a provider
  run for that engine while preserving the same Claire task.
- `/model` shows the current engine's model; `/model MODEL` changes it.
- `/where` shows the active engine, model, effort, node, and memory state.
- `/tasks` shows recent task activity.
- `/remember FACT` writes an explicit durable memory note when OptMem is
  enabled.
- `/new` starts a fresh Claire conversation.
- `/stop` interrupts the current Claire turn.
- `/help` lists the commands.

Only one Claire turn runs at a time. She can still start separate worker tasks,
which nodes may execute concurrently.

### Configure Claire

Open **Claire settings** in the authenticated control panel. The settings are
stored in the hub database and include:

- Claire's name and personality prompt.
- The active engine (`claude` or `codex`).
- A model for each engine and the Codex reasoning effort.
- The node ID and optional absolute workspace used for Claire's conversation.
- Whether OptMem is enabled, its absolute executable path, and an optional
  absolute memory directory.

The Telegram config seeds these settings only the first time:

```json
{
  "control": {
    "telegram": {
      "enabled": true,
      "token": "BOT_TOKEN",
      "chatId": 123456789,
      "nodeId": "local",
      "engine": "claude",
      "workspace": "/home/user/work",
      "claudeModel": "your-claude-model",
      "codexModel": "your-codex-model",
      "personality": "Warm, perceptive, direct, and quietly witty.",
      "memoryCommand": "/home/user/.local/bin/memo",
      "memoryDir": "/home/user/.local/share/stackhour/claire-memory"
    }
  }
}
```

After initialization, use the panel as the source of truth. Model, node, and
path values are validated before Stackhour stores or launches them.

### Optional OptMem memory

Claire can use [OptMem](https://github.com/VictorTaelin/OptMem) as an optional
external memory adapter. Install its `memo` executable separately, then enter
the executable's absolute path in **Claire settings**. Stackhour does not vendor
OptMem or make its memory log authoritative.

At the beginning of a new provider run, Stackhour invokes `memo wake` and
bounds the returned context before including it in Claire's system prompt.
Claire can emit short durable notes, which Stackhour writes with `memo note`.
`MEMORY_DIR` is set only when the optional memory directory is configured.
Stackhour invokes the executable directly, never through a shell. Missing or
failing memory is reported or omitted without preventing task control.

The typed action boundary deliberately excludes credential changes, software
installation, deletion, backup restoration, administrative operations, and
approval decisions.

The old `stackhour bridge` commands remain available as a rollback path. Do not
run the old coordinator and the control hub Telegram client at the same time.

## Delivery and recovery

The hub stores each command before it sends the command to a node. If the node
is offline, the hub sends pending commands after the node reconnects. A node
acknowledgement removes the command from the pending list.

Task creation, run creation, the event, the command receipt, and a related node
dispatch use one SQLite transaction. A restart cannot leave an event without
its task or run.

Clients resume from the last event sequence. Node events use stable event IDs.
The hub can remove duplicate events.

## Current limits

- The node uses the installed Claude and Codex command-line programs.
- The panel does not show all tool activity.
- Durable approval records exist, but the CLI adapter does not stop a tool call
  for approval.
- A node process crash after it accepts a running command can leave that run
  incomplete. The hub keeps commands that the node did not accept. The node
  does not have an on-disk engine outbox.
- Claire's task/run binding survives a hub restart, but a node restart loses
  the CLI process behind an active run. The failed run is cleared so the next
  message can start a new provider run in the same Claire task.
- Claire's first typed-action boundary cannot perform approvals or privileged
  administration.

Keep the old bridge until these limits are acceptable for the live workflow.
`bridge install worker` removes it.

See [bridge.md](bridge.md) for the old bridge guide. See
[../SECURITY.md](../SECURITY.md) for the security model.
