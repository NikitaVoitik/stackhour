# Legacy bridge config fixtures

Input for `stackhour bridge migrate`. These mirror the **shape** of the Node
coordinator's `~/.claude-remote/config.json` — key order, nesting, types, and
which per-target keys are optional. Every value is fake. No real token, chat
id, or API key has ever been in this directory, and none may be added.

`legacy-config.json` — a faithful copy of the shape of a real deployment:

- three targets: two `type: "local"` (`gcp`, `blort`) and one `type: "worker"`
  (`mac`, which carries only `label` / `type` / `permissionMode`);
- `model` present but `null`;
- no `maxMediaBytes` (the coordinator defaults it to 512 MiB in code);
- no `codexBin` / `codexModel` on any target, even though `spawnLocal` can use
  them — the running Node bridge falls back to a hardcoded codex path.

`legacy-config-maximal.json` — every optional key populated: `maxMediaBytes`,
`codexBin`, `codexModel`, a non-null `model`, `permissionMode: "default"`
(the non-bypass branch of `spawnLocal`), an empty `elevenLabsApiKey`
(transcription disabled), and `defaultTarget` pointing at the worker target.

Together they cover both branches of every conditional the migrator reads.
