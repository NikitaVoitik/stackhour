# Working plan — public repository refresh

## Goal

Make Stackhour's public landing page accurately communicate its strongest
capability: a self-hosted control plane for running Claude Code and Codex across
multiple machines, with integrated human and agent activity tracking.

The refresh will:

1. lead the README with the control plane and its concrete capabilities;
2. add a compact architecture overview for readers evaluating the project;
3. publish a short, honest roadmap derived from documented current limits;
4. update the GitHub description and topics after the documentation lands.

This work does not change runtime behavior, versions, release artifacts, or the
documented security model.

## Verification plan

Profile: Standard

Reason:

The public documentation refresh is Markdown-only, but GitHub's Rust 1.98
toolchain exposed a pre-existing Clippy failure in an HTTP helper after the
first push. The narrow lint-policy annotation touches production source, so the
repository's Standard profile is required.

## Planned command

- `dev/verify`

## Verification result

Status: Complete. The Standard verification profile passed with Rust 1.98.

Commands run:

- `pnpm --dir frontend install --frozen-lockfile` to install the locked
  dependencies required by the fresh worktree;
- `dev/verify-fast`, covering Rust formatting, unsafe-code policy, the locked
  all-target/all-feature Rust workspace check, frontend formatting and strict
  TypeScript, and installer shell syntax;
- `RUSTUP_TOOLCHAIN=1.98.0 dev/verify`, covering the Fast profile plus strict
  Clippy, all workspace tests, frontend ESLint, and frontend tests.

The first `dev/verify-fast` attempt could not resolve crates.io inside the
sandbox. The network-enabled retry passed the Rust checks and then identified
the missing frontend dependencies. After installing the locked dependencies,
the final run passed every Fast-profile step.

The Linux Standard CI job then exposed `clippy::result_large_err` on
`read_json`, matching the repository's already documented and allowed
ready-to-send `Response` error pattern in `principal_or_401`. The compatibility
repair adds the same narrow annotation and rationale to `read_json`.

Rust 1.99 adds a separate `assert_is_empty` style lint across existing tests.
That toolchain-wide cleanup is outside this compatibility repair. The Standard
profile was therefore reproduced with Rust 1.98, the compiler generation used
by the failed GitHub job. The first local test run was blocked from opening two
loopback listeners by the macOS sandbox; the permitted rerun passed all Rust
and frontend tests.

Checks not run:

- Full, Deep, and Release profiles were not required for this isolated lint
  compatibility annotation and documentation refresh.
