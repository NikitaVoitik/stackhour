# Stackhour roadmap

Stackhour is an experimental self-hosted project with a working tracker,
multi-machine control plane, web panel, native Claude and Codex adapters, and
hub-resident Telegram assistant. This roadmap records the next engineering
problems; it is not a release schedule.

## Reliability

- Persist a node-side engine event outbox so accepted work can recover cleanly
  after a node process crash.
- Make interrupted hub-local provider sessions resumable across hub restarts.
- Improve disconnected-node diagnostics, retry visibility, and operator-facing
  recovery actions.

## Supervision and security

- Connect durable approval records to the CLI adapters so configured tool calls
  can pause for explicit approval.
- Replace the tracker's query-string dashboard API key with a first-class
  browser session flow.
- Continue tightening workspace, process, update, and Telegram disclosure
  boundaries as additional clients are introduced.

## Operator experience

- Show a more complete, compact tool and file activity timeline in the control
  panel.
- Add clearer machine health, compatibility, and update status.
- Make backup, restore, enrollment, and disaster-recovery paths easier to audit.

## Clients and integrations

- Expand provider-neutral engine support through ACP while keeping native
  Claude and Codex adapters.
- Explore richer desktop and mobile clients after the hub/node protocol and
  recovery model are stable.
- Add optional review surfaces for diffs, terminals, files, and detected preview
  ports without weakening the node workspace boundary.

## Tracking

- Merge imported WakaTime history into dashboard summaries.
- Improve attribution diagnostics when upstream Claude, Codex, or Zed storage
  formats change.
- Add clearer cost-estimate provenance and coverage reporting.

The concrete limitations behind these items are documented in the
[control plane guide](docs/control-plane.md#current-limits) and README.
