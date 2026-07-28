# Agent instructions

## Verification

Select the verification profile before you change files. Record the profile and
the reason in `PLAN.md`. Run the selected command before you report completion.

| Profile | Command | Minimum use |
|---|---|---|
| Fast | `dev/verify-fast` | Documentation, comments, and formatting only |
| Standard | `dev/verify` | Normal isolated code changes |
| Full | `dev/verify-full` | Protocol, authentication, authorization, storage, migrations, concurrency, dependencies, installers, CI, or changes across services |
| Deep | `dev/verify-deep` | Unsafe code, untrusted parsers, security boundaries, or a deliberate deep audit |
| Release | `dev/verify-release` | Version changes and release preparation |

The profile is a minimum. Use a higher profile when the risk is higher. If the
correct profile is unclear, use the next higher profile. A release that also
changes a security boundary must pass both the deep and release-specific checks;
`dev/verify-release` includes both.

Before you report completion, state:

1. The selected verification profile.
2. Why you selected it.
3. The commands that ran.
4. The result and all checks that you did not run.

Do not claim completion if a required check failed. Do not silently skip a check
because its tool is missing. Install the tool or report the missing tool.
Run `dev/bootstrap-verification-tools.sh` to install the full and deep profile
tools.

GitHub CI runs only the fast and standard profiles. Agents run the full, deep,
and release profiles locally when the change requires them.

The same profiles include the frontend:

- Fast: Prettier and strict TypeScript.
- Standard: zero-warning ESLint and unit tests.
- Full: coverage, production build, Knip, dependency-cruiser, size limits, and
  dependency audit.
- Deep: required for Tauri capabilities, permissions, commands, and other
  desktop security boundaries.

## Repository hooks

Claude Code and Codex load repository hooks from `.claude/settings.json` and
`.codex/hooks.json`. The hooks use `dev/agent-verify-hook` and enforce:

1. Verification policy context when a session starts.
2. `dev/verify-fast` after every file edit.
3. The required profile before the main agent can stop.

The Stop hook infers a minimum profile from changed paths and raises it to the
profile recorded in `PLAN.md`. It never lowers the recorded profile. A failed
check returns the failure to the agent so it can repair the change. A second
failure stops the turn and reports that completion is not allowed.

Codex requires review of new or changed project hooks. Open `/hooks`, inspect
the repository definitions, and trust them. Project hooks run only for a
trusted repository.
