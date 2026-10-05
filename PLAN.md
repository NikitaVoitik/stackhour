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

Profile: Fast

Reason:

Only Markdown documentation and repository metadata change. The repository's
Fast profile is the required minimum for documentation-only work.

## Planned command

- `dev/verify-fast`

## Verification result

Status: Complete. The Fast verification profile passed.

Commands run:

- `pnpm --dir frontend install --frozen-lockfile` to install the locked
  dependencies required by the fresh worktree;
- `dev/verify-fast`, covering Rust formatting, unsafe-code policy, the locked
  all-target/all-feature Rust workspace check, frontend formatting and strict
  TypeScript, and installer shell syntax.

The first `dev/verify-fast` attempt could not resolve crates.io inside the
sandbox. The network-enabled retry passed the Rust checks and then identified
the missing frontend dependencies. After installing the locked dependencies,
the final run passed every Fast-profile step.

Checks not run:

- Standard, Full, Deep, and Release profiles are outside the scope of this
  documentation-only refresh.
