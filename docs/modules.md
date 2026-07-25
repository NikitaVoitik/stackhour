# Modules: compile-time and runtime

Stackhour is three products sharing one binary and one config file: a **time
tracker**, a **local agent**, and a **Telegram bridge**. Not every machine
wants all three. A cheap leader VPS runs the bridge and nothing else; a
laptop runs the agent and talks to a server elsewhere.

Modules make that explicit at two layers:

| Layer | Mechanism | Question it answers | Remedy when it refuses |
|---|---|---|---|
| 1 | Cargo features | Is the code in this binary at all? | Rebuild |
| 2 | `"modules"` in `config.json` | Is the operator letting it run? | Edit config |

Both layers resolve through one registry, `stackhour_core::modules`, so help
text, `doctor`, and dispatch can never disagree about what is on.

## The three modules

| Module | Crates | Verbs |
|---|---|---|
| `tracker` | `stackhour-server`, `stackhour-store` | `serve`, `status`, `token`, `data`, `backup`, `import-wakatime`, `init server`, `install server` |
| `agent` | `stackhour-agent` | `agent`, `init agent`, `install agent` |
| `bridge` | `stackhour-bridge` | `bridge *`, plus the hidden `coordinator` / `worker` / `claim` / `return` / `tg-send` wire routes |

`stackhour-core` is the shared spine and is **never** optional: config load
and merge, storage paths, JS-semantics helpers, tokens, the config-directory
registry, and this module registry itself.

`doctor` belongs to no module. It is the diagnostic of last resort, so it is
never gated — it runs, and reports, on every build and under every config.
An unknown verb is likewise never gated: it keeps falling through to the
exit-0 usage banner.

`init` and `install` are gated **by role**, not by verb: `init server` needs
the tracker, `init agent` needs the agent. A bare `init` with no role, or an
unknown role, is not gated at all — the verb owns that error and still prints
`usage: stackhour init <server|agent>` with exit 1.

## Layer 2: the runtime block

```json
{
  "modules": {
    "tracker": true,
    "agent": true,
    "bridge": false
  }
}
```

The gate **fails open**, always. All of these mean "everything enabled":

- no `modules` key at all;
- `"modules": null`, `"modules": 3`, `"modules": []` — any non-object;
- a present block with a sub-key missing, or set to `null`;
- a sub-key this version does not know about (it is ignored).

Only an explicitly present, JS-falsy value turns a module off: `false`, `0`,
`""`.

**`"bridge": "false"` ENABLES the bridge.** A non-empty string is truthy in
JavaScript, and every other toggle in this config reads through the same
`Boolean(v)` coercion inherited from the config's JavaScript origin. No
warning is emitted. Write
the bare `false` literal, not a quoted one.

`modules` is deliberately **absent from the defaults table**, exactly like
`pricing`. Two consequences worth knowing:

- a config with no `modules` key produces a `config.json` byte-identical to
  one written before modules existed — no new root key, no key-order change;
- a user block lands in the merged config **verbatim** rather than merging
  per-key with a built-in one.

### Why the gate reads the file itself

The dispatch gate runs at the top of `main()`, before the verb-specific
`load_config()`. It uses a **lenient** read of the user's `config.json`: a
missing file, an unreadable file, or malformed JSON all resolve to
"everything enabled". That read never allocates an error and never fails.

This is what preserves the corrupt-config ordering contract. A broken
`config.json` still fails inside the verb, with today's message and exit 1 —
not at the gate with a confusing exit 2. And reading the user file rather
than the merged config is exact, precisely because `modules` is absent from
the defaults.

## Layer 1: build without a module

```sh
cargo build -p stackhour --no-default-features --features bridge   # cheap leader VPS
cargo build -p stackhour --no-default-features --features agent    # worker box
cargo build -p stackhour --no-default-features --features tracker,agent
```

`default = ["tracker", "agent", "bridge"]`, so a plain `cargo build` is
unchanged in every respect.

The feature names and the config sub-keys are deliberately the **same three
strings**, so one identifier names both layers.

### What a bridge-only build actually drops

20 crates leave the dependency closure (134 → 114 unique packages on normal
dependency edges), including `axum`, `axum-core`, `matchit`, `rusqlite`,
`libsqlite3-sys` and the bundled SQLite C amalgamation, `hashlink`,
`fallible-iterator`, and the three Stackhour crates themselves.

**tokio is still linked.** The bridge needs `reqwest::blocking` for Telegram
long-polling, and that pulls tokio and hyper. Do not claim otherwise.

An **agent-only** build is likewise not SQLite-free: `stackhour-agent`
depends on `rusqlite` in its own right for the Zed `threads.db` snapshot.
Only a bridge-only build has no SQLite in it.

### Feature / crate matrix

| Feature | Pulls in | Also links |
|---|---|---|
| `tracker` | `stackhour-server`, `stackhour-store` | `rusqlite`, `axum` |
| `agent` | `stackhour-agent` | `rusqlite` |
| `bridge` | `stackhour-bridge` | `reqwest::blocking` → tokio, hyper |
| (always) | `stackhour-core` | `serde_json`, `reqwest` |

`reqwest` is **not** optional in the `stackhour` binary: `status` and
doctor's `server-auth` check both use the blocking client, and neither
belongs to a module.

## Resolution order: compile-time first

When a module is both uncompiled and disabled, the **compile-time** message
wins. Recompiling is the only remedy, so sending the operator to
`config.json` would be the wrong instruction.

```
stackhour: bridge needs the bridge module, which was not compiled into this binary (rebuild with --features bridge)
```

```
stackhour: serve needs the tracker module, which is disabled by "modules.tracker": false in /home/you/.config/stackhour/config.json
```

Both are single-line, both go to **stderr**, and both exit **2**.

Which one means what:

- `not compiled into this binary` → **rebuild** with the named feature. The
  message never mentions a config key, because editing config cannot fix it.
- `disabled by "modules.X": false` → **edit config**. The message names the
  dotted key and the absolute path of the file to edit.

Grep `needs the .* module` to catch both; grep `not compiled into this
binary` to isolate Layer 1.

### Exit code 2 is not unique to the gate

`bridge migrate` already exits 2 when destinations exist, `bridge return`
when the job id is missing, and `bridge tg-send` when the bridge config is
unreadable. The **code** is ambiguous; the **messages** are deliberately
disjoint. Match on the message, not the code.

## What the surfaces say

### Help

The usage banner is byte-identical to what it has always been — the verbs of
a disabled module stay listed. Module state is one appended `note:` line per
off module, after the `config: <path>` line, on **stdout**, and only when
something is off:

```
config: /home/you/.config/stackhour/config.json

note: the bridge module is disabled by "modules.bridge": false; its commands above exit 2.
```

With every module on, nothing is appended at all.

### doctor

`stackhour doctor` appends one `module-<name>` line per off module, after
every existing check:

```
✓ module-tracker: disabled by "modules.tracker": false in /home/you/.config/stackhour/config.json
✓ module-bridge: not compiled into this binary (rebuild with --features bridge)
```

The status is always `✓`. A deliberate operator choice is not a fault, and an
error here would make a healthy bridge-only leader start exiting 1. `--json`
carries these as ordinary `checks[]` entries — the document keeps its exact
`{ok, version, checks}` shape and `checks[0].name` is `runtime`.

A build without the `tracker` feature also has **no `database` check**. The
`sqlite` check goes away only when NEITHER `tracker` nor `agent` is compiled
in — it is gated on the two modules that link rusqlite, so the agent-only
worker box build still reports `sqlite` and only the bridge-only leader
build, which links no SQLite at all, omits it. Those absences are expected,
and the `module-*` lines are what tell you so rather than leaving you to
debug a broken install.

Note that `doctor` reports the disabled module and then reports the
consequences anyway: a box with `"tracker": false` still gets its
`server-auth` check, which will fail because nothing is serving. The module
line is the explanation, not a filter.

## Installing services with a module off

`install server` installs two units — `stackhour-server` **and**
`stackhour-agent` — owned by two different modules. The verb is gated on the
tracker (the module the invocation belongs to), but each unit is checked
against its own module before being started:

```
Skipped stackhour-agent: the agent module is disabled by "modules.agent": false in /home/you/.config/stackhour/config.json
Installed and started stackhour-server
```

The wording is `Skipped`, never `needs`, and the command still succeeds — one
unit being off is not a failure of the install. Without this second check,
`install server` on a box with `"agent": false` would report success while
leaving a `Restart=always` / `RestartSec=10` unit whose every start the gate
refuses with exit 2: a crash loop every ten seconds, forever.

`init server --install` and `init agent --install` route through the same
table, so the two entry points can never disagree.

## Adding a fourth module

The registry itself lives in `crates/stackhour-core/src/modules.rs`:

1. add the variant to `Module` and to `Module::ALL`;
2. give it a `name()` — the same string is the Cargo feature and the config
   sub-key — and a `config_key()`, which must be exactly `modules.` + that
   name. The exhaustive match catches a MISSING arm; only the test
   `every_config_key_is_the_module_name_under_modules` catches a typo'd one,
   and a typo'd key would ship an error message and a `module-*` doctor line
   pointing at a config key that does nothing;
3. add the field to `ModuleSet` (and to `new`, `contains`, `from_raw`);
4. map its verbs in `module_for`, and its service units, if any, in
   `service_roles_for`;
5. add the feature to `crates/stackhour/Cargo.toml`, wire it into
   `compiled_modules()` in `main.rs`, and `#[cfg]` its dispatch arms;
6. add the reduced-feature build to `.github/workflows/rust.yml`.

Then widen the hand-written feature predicates in the test tree. No registry
can reach a `#[cfg]`, so these are edited by hand or they rot:

* `crates/stackhour/tests/not_compiled_gate.rs` — the file-level
  `#![cfg(not(all(...)))]` and the `cfg!` chain in `missing_modules()`. Both
  must gain the new feature. This is the dangerous pair: miss it and Layer-1
  coverage for every build that omits the new module silently disappears
  instead of failing;
* `check_order_matches_the_documented_inventory` (`doctor_checks.rs`) and
  `a_default_build_compiles_in_every_module` (`main.rs`) — `#[cfg(all(...))]`
  pinned to a full build; they must gain the new feature or they start
  running, and failing, in a build that omits it;
* the file-level `cfg`s on `tests/modules_gate.rs`, `tests/cli.rs` and
  `tests/agent.rs`, and the two `#[cfg(all(feature = "tracker", feature =
  "agent"))]` tests in `init.rs` — same rule; these fail loudly rather than
  silently, but they do fail.

What genuinely needs no per-module edit is everything that reads the
registry at runtime: the help-text filter, `doctor`'s `module-*` lines,
`install`'s per-unit check, and the top-of-`main` dispatch gate all iterate
`Module::ALL`.
