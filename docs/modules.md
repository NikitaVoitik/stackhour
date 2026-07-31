# Modules

Stackhour ships one binary with three optional modules:

| Module | Cargo feature | Main commands |
| --- | --- | --- |
| Tracker | `tracker` | `serve`, `status`, `token`, `data`, `backup`, `migrate tempo`, `import-wakatime` |
| Activity agent | `agent` | `agent`, `init agent`, `install agent` |
| Control plane | `control` | `control hub`, `control node`, `control fake-telegram`, `control install`, `control update` |

All three features are enabled by default. Reduced binaries can select only
the modules they need:

```sh
cargo build -p stackhour --no-default-features --features tracker
cargo build -p stackhour --no-default-features --features agent
cargo build -p stackhour --no-default-features --features control
```

The runtime `modules` object can disable a compiled module:

```json
{
  "modules": {
    "tracker": true,
    "agent": true,
    "control": false
  }
}
```

Missing keys and malformed blocks fail open so older configuration files keep
working. Values follow the application's established JavaScript-compatible
truthiness rules; use the JSON literal `false`, not the string `"false"`.

The gate runs before command dispatch. A command from an unavailable module
exits with status 2 and writes one diagnostic line:

```text
stackhour: control needs the control module, which was not compiled into this binary (rebuild with --features control)
stackhour: serve needs the tracker module, which is disabled by "modules.tracker": false in /home/user/.config/stackhour/config.json
```

`doctor` and the help screen are always available. Doctor reports each
unavailable module as an informational `module-<name>` check.

`install server` creates the server service when tracker is available and the
activity-agent service when agent is available. A disabled or uncompiled
activity agent is skipped with an explicit message.
