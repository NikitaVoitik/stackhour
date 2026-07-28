# Working plan

## Verification plan

Profile: Deep

Reason:

This change removes direct unsafe Rust, forbids future unsafe code, makes
warnings fatal, increases Clippy strictness, and adds a strict verification
contract for the future desktop frontend. It changes safety and dependency
boundaries, so it requires Deep verification.

Affected areas:

- Claude Code project configuration
- Codex project configuration
- Shared agent-hook runner
- Verification-profile selection
- Rust safety and lint policy
- POSIX process, terminal, and file-time wrappers
- Future Tauri and TypeScript frontend verification

Additional focused checks:

- Hook configuration parsing
- Profile selection from changed paths
- PLAN.md profile escalation
- Failed-verification stop behavior
- Repository unsafe-code scan
- Fatal Rust warnings and strict Clippy groups
- Strict TypeScript, ESLint, formatting, tests, coverage, and dependency checks

## Verification result

Status: Passed

Commands run:

- `dev/verify-deep`
- `cargo check --offline --workspace --all-targets --all-features`
- `cargo clippy --offline --workspace --all-targets --all-features -- -D warnings`
- `cargo clippy --offline --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings`
- `dev/check-unsafe.sh`
- `dev/verify-frontend fast`
- `dev/verify-frontend standard`
- `dev/verify-frontend full`
- `dev/test-verify.sh`
- `dev/test-agent-hooks.sh`
- `dev/test-coverage.sh`
- `cargo deny check`
- `cargo deny --manifest-path fuzz/Cargo.toml --config fuzz/deny.toml --offline check`
- `pnpm --dir frontend install --frozen-lockfile --offline`
- `pnpm --dir frontend exec vite preview --host 127.0.0.1 --port 4173`
- `curl -fsS http://127.0.0.1:4173/`
- `git diff --check`

Checks not run:

- `dev/verify-release`

Reason for each omitted check:

- No version or release package changed. Deep includes all required safety,
  coverage, Miri, fuzz, mutation, dependency, installer, and frontend checks.
