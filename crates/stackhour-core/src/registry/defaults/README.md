# stackhour bridge configuration

This directory extends the Telegram bridge WITHOUT touching any code.

Everything here is optional. Delete the whole tree and the bridge behaves
exactly as it does out of the box — the shipped commands, agents, engines and
prompts are compiled into the binary and this directory only *overrides* them.

    config.json          # the legacy settings file; unchanged.
                         # Its optional "bridge" object holds scalar defaults.
    commands/<name>.toml # one file per command; the file stem is the verb
    skills/<name>/       # skill.toml + skill.md
    agents/<name>/       # agent.toml + soul.md (+ overlays)
    engines/<name>.toml  # extra CLI engines; claude and codex ship built in
    prompts/<name>.md    # override a shipped template, or define a new one

Rules that apply everywhere:

* The entity NAME is the file stem (flat entities) or the directory name
  (agents and skills). It is never repeated inside the file.
* Dot-prefixed files and directories are ignored, so editor scratch files and
  `.git` never load.
* An override REPLACES the shipped entity of the same name wholesale — there
  is no field-level merging. The one exception is an agent's `extends`, which
  is inheritance you asked for explicitly.
* A file that fails to parse or validate is SKIPPED, never fatal. The reason
  is printed by `stackhour bridge doctor`, naming the file and the key.
* Prose files (soul.md, skill.md, prompts/*.md) are re-read when their mtime
  changes, so editing them takes effect on the very next message — no reload,
  no restart. Adding or removing a FILE is picked up before the next Telegram
  update is dispatched.

Precedence, lowest to highest:

  1. built into the binary
  2. the "bridge" object in config.json
  3. the files in this directory
  4. environment: STACKHOUR_CONFIG_DIR, STACKHOUR_AGENT,
     STACKHOUR_ENGINE, STACKHOUR_TARGET
