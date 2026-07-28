# Working plan

## Verification plan

Profile: Full

Reason:

This change repairs GitHub CI on both supported runner platforms. It changes
dependency installation, macOS test compilation, and verification-tool setup,
so it requires Full verification.

Affected areas:

- Frontend patch metadata
- Frontend dependency lockfile
- GitHub CI dependency installation
- GitHub Action runtime compatibility
- Platform-gated Rust test helpers
- Portable unsafe-scan tool selection

Additional focused checks:

- A clean frozen pnpm installation
- The frontend patch applies from a clean dependency store
- The unsafe scan uses a strict grep fallback when ripgrep is unavailable
- GitHub CI on Linux and macOS

## Verification result

Status: Passed

Commands run:

- `pnpm --dir frontend install --no-frozen-lockfile`
- `pnpm --dir frontend install --frozen-lockfile`
- Clean temporary `pnpm install --frozen-lockfile` with an empty store
- Clean grep-fallback scan with a restricted `PATH`
- Grep-fallback detection check with a temporary unsafe fixture
- `dev/check-unsafe.sh`
- `cargo fmt --all --check`
- `dev/verify-fast`
- `git diff --check`
- `dev/verify-full` with loopback socket access
- `dev/verify-full` after Linux CI tool setup
- `dev/verify-full` after the GitHub Action runtime upgrade
- `dev/verify-full` after the portable scanner change

Checks not run:

- `dev/verify-deep`
- `dev/verify-release`

Reason for each omitted check:

- The change does not modify unsafe code, an untrusted parser, or a security
  boundary, so Deep is not required.
- The change does not modify a version or prepare a release, so Release is not
  required.
