# Working plan

## Goal

Add authenticated in-app and automatic updates for the Stackhour control plane,
using only official checksum-verified release artifacts. While doing so, repair
the release, installer, service, doctor, target-UX, migration, and operational
gaps documented in the attached live-deployment report.

The updater must update the coordinator and connected execution nodes without
accepting arbitrary download URLs or shell commands from the browser. Update
policy and status are durable and manageable from the control panel.

## Verification plan

Profile: Release

Reason:

This change modifies release targets and artifacts and also crosses
authenticated administration APIs, automatic downloads, checksum validation,
binary replacement, service restart, process launch, the node wire protocol,
durable control settings, installers, migrations, and generated system
services. Release verification is required for release preparation and includes
the Deep security checks required for these privileged and untrusted-input
boundaries.

Affected areas:

- Portable Linux release builds and artifact smoke tests
- Checksum-verified self-update and release discovery
- Authenticated control-panel update policy, status, and manual update API
- Coordinator and connected-node update dispatch
- systemd and launchd service rendering and multi-worker identities
- Installer post-start health verification
- Doctor authentication, runtime-module, and watcher-aware checks
- Machine/workspace target presentation
- Bridge relocation and Tempo migration/operator workflows
- Secure remote-agent connectivity documentation and configuration

Focused checks:

- Linux artifacts execute on Debian 12 and Amazon Linux 2023
- No browser-provided value can select an update URL, executable, or shell text
- Release archives are installed only after SHA-256 verification
- Manual and automatic updates require the configured client credential
- Update scheduling is bounded, durable, single-flight, and disabled by default
- Node update work is authenticated, expires, acknowledges, and is idempotent
- Existing hub/node services restart from the replaced stable binary
- Custom runtime directories survive in every installed service definition
- Multiple bridge workers on one machine get distinct validated identities
- Generated systemd units validate on supported Linux distributions
- Doctor reuses authentication and skips disabled or inapplicable checks
- Installers report failure when a freshly started service is unhealthy
- Migrations are transactional, preserve their source, and verify row/state
  counts before reporting success
- Protocol-version mismatch remains explicit across rolling upgrades

## External operational follow-ups

The attached AWS-root credentials, MFA, security-group, Elastic IP, disk
cleanup, and live Tailscale/WireGuard changes are not repository mutations and
will be reported separately. No cloud account or live host will be changed
without explicit authorization and credentials in scope.

## Repository follow-ups not implemented

- A transactional, remote `bridge relocate` cutover/rollback orchestrator.
  Existing migration and installer primitives do not yet update every worker
  destination and prove a remote leader before retiring the old one.
- A managed SSH tunnel or TLS/Tailscale endpoint workflow for `init agent`.
- Engine-level approval interception. Durable approval commands/events exist in
  the control-plane protocol, but the current CLI adapter does not yet suspend
  an active provider tool call while it waits for a remote decision.

## Verification result

Status: Passed

Commands run:

- `dev/bootstrap-verification-tools.sh` — installed the Release/Deep verification
  toolchain successfully.
- `cargo test -p stackhour-domain --all-features`
- `cargo test -p stackhour-hub --all-features`
- `cargo test -p stackhour-node --all-features`
- `cargo test -p stackhour --test bridge_operator_cli`
- `cargo test -p stackhour control_update`
- `cargo test -p stackhour tempo_migrate`
- `cargo test -p stackhour doctor`
- `cargo clippy --workspace --all-targets --all-features`
- `sh deploy/test-install.sh`
- `dev/test-systemd-units.sh`
- `dev/verify-fast`
- `dev/verify`
- `dev/verify-release`

Result:

The Release profile passed. It includes formatting, strict Clippy, all workspace
and frontend tests, frontend production build and audit, feature-matrix builds,
minimum-Rust-version verification, dependency/license/source policy, shell and
workflow analysis, secret scanning, installer tests, coverage, Miri, 61 seconds
and 3,124,711 executions of protocol fuzzing, mutation tests, generated systemd
unit validation, optimized release builds/tests, and release packaging.

Checks not run locally:

- The Debian 12 and Amazon Linux 2023 artifact smoke tests require the
  architecture-specific musl artifacts produced by GitHub Actions. Their
  commands and service-unit validation are wired into the release workflow but
  were not executed against downloaded CI artifacts on this host.
- No update was applied to a live installed hub or node, and no production
  service was restarted.
- No AWS, firewall, Elastic IP, disk, Tailscale, or WireGuard operation was
  performed.
