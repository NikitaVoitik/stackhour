# Working plan — owner ideas 1, 3, 4, 7, and 8

## Goal

Implement the selected owner ideas in ordered, reviewable stages:

1. keep time tracking direct to the tracking server, separate from the control
   hub, independently disableable, and size-conscious;
2. let the control-plane API schedule worker tasks on eligible active execution
   nodes;
3. keep Claire exclusively on the hub and make Telegram talk only to that
   hub-resident Claire, without a node selector;
4. persist worker completion and failure events that wake Claire so she can
   choose the next action and whether to notify the user;
5. make Claire the sole policy owner for Telegram disclosure of progress,
   questions, tool activity, and results.

After each implementation stage, run the selected repository verification and
launch a fresh Claude CLI review with Opus and high effort using
`/code-review`. Address actionable findings before moving to the next stage.

Finally, add a fake Telegram UI/API suitable for local end-to-end testing and
launch low-context Claude CLI agents as ordinary users. They must exercise the
app, inspect time-tracking results, and report whether attribution and duration
are correct.

## Verification plan

Profile: Deep

Reason:

This work changes control-plane APIs, authentication and authorization
behavior, hub/node task routing, durable event storage, Telegram disclosure
boundaries, process execution, tests, and user-facing simulation tooling across
multiple services. Those are security boundaries and untrusted-input paths, so
Deep is required.

Focused checks:

- tracker traffic remains direct from activity agents to the tracking server
  and can be independently disabled;
- tracker-only, agent-only, control-only, and combined feature builds remain
  valid and size limits pass;
- only authenticated eligible active nodes can receive worker tasks;
- Claire and the Telegram transport remain hub-only, with no node selector or
  alternate-Claire routing;
- worker terminal events are durable, idempotent, and wake Claire after restart;
- Claire owns Telegram disclosure decisions and raw execution details cannot be
  enabled by a user-facing setting;
- the fake Telegram surface cannot weaken production authentication;
- ordinary-user scenarios cover success, failure, cancellation, restart, and
  concurrent worker activity;
- human and agent time remain separately attributed without double counting.

## Planned commands

- focused Rust and frontend tests after each stage;
- `dev/verify-fast` after edits;
- `dev/verify-deep` for each completed security-boundary stage and once at the
  end;
- `claude --model opus --effort high -p "/code-review ..."` after each stage;
- low-context `claude --model sonnet --permission-mode dontAsk -p ...`
  ordinary-user runs
  against the fake Telegram UI/API.

## Verification result

Status: Complete. Implementation, reviews, ordinary-user audit, and the final
Deep verification profile all passed.

Commands run:

- repeated focused `cargo test` runs for `stackhour-domain`,
  `stackhour-hub`, `stackhour-node`, the `stackhour` control and Telegram
  modules, CLI integration tests, and the real hub/node worker-wake path;
- repeated `cargo clippy --workspace --all-targets --all-features -- -D
  warnings`;
- repeated `dev/verify-fast`;
- final `dev/verify-deep`;
- Opus/high `/code-review` passes after direct tracking, worker scheduling and
  hub-only Claire, durable wakes, Claire-owned disclosure, and fake Telegram;
  all high/medium actionable findings were repaired and rechecked;
- three low-context Claude CLI black-box user journeys against the loopback
  fake Telegram API/UI and isolated direct tracker;
- isolated live services: tracker `127.0.0.1:4140`, hub
  `127.0.0.1:4150`, fake Telegram `127.0.0.1:4160`, and one eligible
  execution node.

Ordinary-user result:

- greeting, help, worker creation/completion, `/where`, `/tasks`, unknown
  commands, sanitized result projection, and sequential messaging worked;
- no raw worker/tool output or internal errors reached Telegram;
- the clean before/after audit measured `claude-code`/`agent` increasing from
  405s to 444s (+39s) over a 42s wall-clock window;
- attribution remained `NikitaVoitik/stackhour` on `audit-machine`, with
  `claude-code`/`agent` separate from `codex-desktop`/`human`.

Checks not run:

- `dev/verify-release` was not run because this work does not change a version
  or prepare a release. No required checks were skipped.
