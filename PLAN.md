# Working plan

## Verification plan

Profile: Full

Reason:

This change repairs a frozen frontend dependency installation in GitHub CI.
It changes the dependency lockfile and CI installation boundary, so it
requires Full verification.

Affected areas:

- Frontend patch metadata
- Frontend dependency lockfile
- GitHub CI dependency installation

Additional focused checks:

- A clean frozen pnpm installation
- The frontend patch applies from a clean dependency store
- GitHub CI on Linux and macOS

## Verification result

Status: Passed

Commands run:

- `pnpm --dir frontend install --no-frozen-lockfile`
- `pnpm --dir frontend install --frozen-lockfile`
- Clean temporary `pnpm install --frozen-lockfile` with an empty store
- `dev/verify-full`

Checks not run:

- `dev/verify-deep`
- `dev/verify-release`

Reason for each omitted check:

- The change does not modify unsafe code, an untrusted parser, or a security
  boundary, so Deep is not required.
- The change does not modify a version or prepare a release, so Release is not
  required.
