# T3 Code UI and functionality study

- Status: reference analysis, not an implementation specification
- Reviewed: 2026-07-24
- T3 Code snapshot: [`ece05087a70e94efcd57441337fa1249559362ba`](https://github.com/pingdotgg/t3code/tree/ece05087a70e94efcd57441337fa1249559362ba)

## Scope

This is a partial source-level study of T3 Code as a product and architecture
reference for Stackhour. It focuses on:

- the desktop, web, and mobile information architecture;
- the task, conversation, approval, diff, terminal, and preview interactions;
- local and remote execution environments;
- the boundary between clients, shared contracts, and the execution server;
- patterns Stackhour should adopt, adapt, or reject.

It is intentionally **not** a plan to copy T3 Code. T3 Code is the closest
current reference, but Stackhour has a different center of gravity: one
personal control plane spanning a laptop, remote development servers,
Telegram, desktop, web/PWA, and eventually mobile.

The review covered the repository documentation, package manifests, contracts,
shared client runtime, principal web and mobile screens, desktop/SSH code, and
server subsystems. It did not attempt exhaustive behavioral testing or a
pixel-by-pixel visual audit.

## Executive findings

1. **The execution environment is T3 Code's most useful primitive.** One
   environment owns its projects, files, Git state, terminals, provider
   processes, and agent sessions. Every resource reference is scoped by an
   `environmentId`. Stackhour needs the same explicit location boundary, named
   `node`, but should place it behind one durable hub rather than asking each
   client to connect independently to every node.

2. **The timeline is the product.** Chat messages are only one type of durable
   thread state. Tool activity, approvals, questions, plans, changed files,
   checkpoints, errors, and terminal context all appear in the same work
   history. Stackhour should model this as structured events from the start;
   reconstructing it later from process output would be brittle.

3. **Git is a first-class interaction surface, not an integration hidden in
   settings.** T3 Code exposes branches, worktrees, per-turn changes, full
   diffs, review comments, commit/push, and pull-request actions beside the
   conversation. This is appropriate for Stackhour too.

4. **Remote and reconnect behavior are product behavior.** T3 Code keeps
   cached shell/thread snapshots, separates transport health from data-sync
   health, retries transient failures, and scopes operations to an environment.
   Stackhour should reuse those ideas, but its nodes should normally maintain
   outbound connections to a hub so sleeping laptops, NAT, Telegram, and web
   clients all share one routing model.

5. **Shared contracts and shared client behavior are worth adopting.** T3 Code
   separates schema-only contracts from client connection/state logic and from
   platform UI. Stackhour's Rust core should own the protocol/domain contracts;
   generated TypeScript types should keep React clients honest.

6. **Do not inherit T3 Code's component scale.** At the reviewed snapshot,
   `ChatView.tsx` is about 6,000 lines, `Sidebar.tsx` about 3,600, and
   `ChatComposer.tsx` about 2,700. Stackhour should organize by feature and
   state machine before the UI grows.

7. **A PWA should precede a native mobile app.** T3 Code's native client shows
   the cost of full parity: native Markdown, diff, terminal, composer, widgets,
   notifications, adaptive tablet layout, and platform-specific code. A
   responsive PWA can cover monitoring, messages, approvals, voice input, and
   lightweight review much sooner. Native mobile becomes justified only when
   notifications, background work, share extensions, or terminal performance
   prove important enough.

## T3 Code product surface

### Desktop and web shell

The primary desktop layout is a three-part workbench:

```text
┌────────────────────┬────────────────────────────┬──────────────────────────┐
│ Projects / threads │ Thread timeline            │ Optional work surface    │
│                    │                            │                          │
│ status and recency │ messages and work log      │ diff / terminal / plan   │
│ environment badges │ changed-file summaries     │ browser preview / files  │
│ create/archive     │ approvals and questions    │                          │
├────────────────────┴────────────────────────────┴──────────────────────────┤
│ Composer: context + attachments + provider/model + mode + access + send   │
└───────────────────────────────────────────────────────────────────────────┘
```

The current marketing screenshot and the source agree on the main hierarchy:

- **Left sidebar:** searchable projects containing threads, current work
  status, pull-request/terminal/preview indicators, environment badges,
  creation, rename, archive, settle/snooze, grouping, sorting, and recency
  limits.
- **Thread header:** title plus branch/worktree/environment context and
  source-control actions.
- **Timeline:** user and assistant messages, compact work groups, active-work
  timer, proposed plans, changed-file trees, attachments, review comments,
  copy/revert actions, and a minimap for long histories.
- **Composer:** rich prompt editing, file mentions, pasted/selected images,
  terminal excerpts, browser annotations, review comments, slash commands,
  provider/model selection, reasoning/traits, Build/Plan mode, runtime access
  mode, stop/send actions, pending approvals, and multi-question input.
- **Right panel:** tabbed diff, terminal, plan, browser preview, and file
  surfaces. Panels can be resized or maximized, and terminal/browser sessions
  can coexist as tabs.
- **Settings:** general behavior, providers and models, connections, source
  control, keybindings, diagnostics, beta features, and archived threads.

This is denser than a conventional chat UI. The density works because the
conversation remains central while specialist surfaces open beside it instead
of navigating away.

### Thread lifecycle

A T3 thread is durable and broader than a provider session. It includes:

- messages and attachments;
- normalized activities such as tools, approvals, and failures;
- active/previous turns;
- provider and model selection;
- interaction and runtime modes;
- a worktree or checkout context;
- checkpoints and per-turn diffs;
- proposed plans;
- archived, settled, and snoozed states;
- zero or more terminal sessions.

The provider process may start, reconnect, stop, or fail without redefining the
thread's identity. That separation is important for Stackhour: a task must
survive engine restarts, node sleep, client disconnects, and switching between
Telegram and a graphical client.

### Work log and messages

T3 Code normalizes provider-native output into a provider-neutral timeline.
The UI derives rows for:

- user messages;
- streaming or complete assistant messages;
- grouped tool/work activities;
- current "working" state and duration;
- proposed plans;
- changed-file summaries;
- pending approval and user-input state.

The UI deliberately compresses routine tool traffic. A coding-agent app should
not render every protocol packet as an equally prominent chat bubble. The
useful hierarchy is:

1. user intent and agent outcome;
2. exceptional or actionable events;
3. expandable execution detail.

Stackhour should retain raw engine events for diagnostics while projecting a
smaller stable set of UI event types.

### Composer as command center

The composer is the richest single surface. It combines:

- natural-language input;
- files and images;
- terminal selections;
- code-review comments;
- browser element/screenshot annotations;
- slash commands;
- provider, model, and model options;
- Build versus Plan interaction mode;
- supervised/automatic/full-access execution policy;
- answers to agent questions and approval decisions.

The important lesson is not to implement all controls immediately. It is to
define one **task input envelope** capable of carrying text plus typed context.
Telegram text, a PWA image upload, a selected diff range, and a desktop
terminal excerpt should all become the same Rust-domain command.

### Git, checkpoints, and review

T3 Code captures hidden Git checkpoints around turns and derives:

- changed-file summaries in the timeline;
- per-turn diffs;
- a full-thread diff;
- split or stacked diff rendering;
- selectable comparison bases;
- file-level navigation;
- checkpoint reversion;
- review comments that are appended to the next agent prompt.

It also has provider-neutral source-control services for GitHub, GitLab,
Bitbucket, and Azure DevOps, with branch, commit, push, and change-request
flows.

This is a strong reference for Stackhour, with two changes:

- checkpointing must be explicit about dirty working trees and generated files;
- restore must always be a clearly described, confirmable operation with an
  audit event, never an opaque "undo."

### Terminal, files, and browser preview

The web UI uses xterm for persistent per-thread terminal sessions. Sessions
support open/attach/write/resize/clear/restart/close, output replay after
reconnect, multiple tabs, split groups, link detection, and adding selected
terminal text to the composer.

File surfaces provide a workspace tree, text/Markdown/image preview, editing,
drag-to-mention, and review annotations.

The browser preview is desktop-only at the reviewed snapshot. Electron owns a
Chromium `webview` and Playwright-backed automation/element-picking layer,
while the server tracks per-thread preview sessions and discovered ports. This
cannot be copied directly into Tauri:

- a Tauri WebView is part of the application shell, not an Electron-style
  arbitrary guest `webview`;
- remote preview URLs require careful proxying, origin, cookie, and CSP rules;
- element picking and agent browser automation need a separate capability and
  security design.

For Stackhour, browser preview should be a later desktop capability. The first
version can expose detected ports as links and use an external browser.

### Mobile

The React Native/Expo app is not merely the responsive web UI in a wrapper. It
has dedicated:

- home/project/thread lists and swipe actions;
- new-task and thread-detail flows;
- approvals and agent questions;
- Git sheets and progress overlays;
- review/diff navigation and review-comment composition;
- workspace file browsing and source rendering;
- native terminal surfaces;
- connection and pairing screens;
- phone/tablet adaptive panes;
- notifications, live activities, widgets, share intake, and shortcuts.

It also carries native modules for Markdown selection/rendering, review diffs,
the composer, and a Ghostty-based terminal. This validates the recommendation
to begin Stackhour mobile access as a PWA. Native parity is a separate product,
not a packaging step.

## T3 Code architecture

### Server boundary

T3 Code's server owns:

- orchestration and persistence;
- provider adapters and live provider processes;
- projects, worktrees, filesystem, and Git;
- checkpoints and diffs;
- PTY processes and output;
- source-control providers;
- preview port discovery and session metadata.

Clients use typed RPC requests, durable subscriptions, and ordered push events.
Provider-specific streams are normalized before reaching UI state.

This is the correct execution boundary. Filesystem paths, Git commands, PTYs,
and provider credentials belong to the machine that executes them.

### Contracts and event projection

The schema-only contracts package defines commands, events, projections,
errors, and environment-scoped identifiers. Orchestration follows an
event-oriented shape:

```text
client command
    → invariant check / decider
    → persisted domain event
    → projection
    → ordered subscription update
    → client state
```

Background reactors perform provider calls, checkpoint capture, and cleanup.
Typed "receipts" signal asynchronous milestones such as checkpoint completion
or a fully quiet turn, avoiding timing-based polling in tests.

Stackhour should adopt the distinction between:

- a **command receipt**: the hub accepted or rejected the request;
- **domain events**: durable facts about what happened;
- **runtime signals**: ephemeral coordination inside one process;
- **projections**: UI-ready current state.

### Shared client runtime

Web and mobile share a client-runtime package that owns:

- known environments and endpoint resolution;
- authentication and credential persistence;
- connection supervision and retry;
- RPC sessions and subscriptions;
- cached shell/thread state;
- focused domain state modules.

Its most useful details are:

- one retry owner per environment;
- connectivity-aware backoff, capped at 16 seconds;
- explicit wakeups for network/app/credential changes;
- cached state remains readable offline;
- transport health and domain synchronization health are separate;
- a new live generation cannot be overwritten by stale cache hydration;
- React components do not construct sockets or retry loops.

Stackhour's TypeScript client runtime should follow these ownership rules even
if the underlying protocol and topology differ.

### Remote execution

T3 Code models an execution environment independently from how it is reached.
Possible access paths include:

- direct HTTP/WebSocket;
- LAN or Tailscale endpoints;
- HTTPS/WSS tunnels;
- an Electron-managed SSH launch plus local port forwarding;
- a managed relay endpoint.

Pairing exchanges a one-time credential for a session. The hosted web app
stores environments locally and connects directly to their backends; it is not
a proxy or central control plane.

That last point is the main mismatch with Stackhour. Direct per-environment
connections are elegant for T3 Code, but weaker for Stackhour's desired
experience:

- Telegram already needs an always-on coordinator;
- a browser should not need credentials and network reachability for every
  development box;
- a laptop behind NAT or asleep should reconnect without changing client
  configuration;
- one task history should remain available when its execution node is offline;
- future scheduling across nodes belongs in one place.

## What Stackhour should adopt, adapt, or reject

| T3 Code pattern | Stackhour decision | Reason |
|---|---|---|
| Environment-scoped resources | **Adopt as `nodeId`** | Prevents path, process, terminal, and Git ambiguity across machines. |
| Server owns execution resources | **Adopt** | The node containing the checkout must own filesystem, Git, PTY, and engine processes. |
| Durable thread above provider session | **Adopt** | Tasks must survive reconnects, restarts, and client switching. |
| Typed command/event/projection contracts | **Adopt** | Required for several clients and an agent-written codebase. |
| Ordered event delivery and replay | **Adopt** | Enables deterministic UI state and reconnect. |
| Cached offline shell/thread state | **Adopt** | Essential for mobile/PWA and sleeping nodes. |
| Worktree per thread | **Adapt** | Offer `existing checkout`, `new worktree`, and later `ephemeral clone`; do not force one mode. |
| Per-turn checkpoints and diffs | **Adopt incrementally** | High product value, but restore safety needs explicit design. |
| Timeline with compressed tool activity | **Adopt** | Preserves clarity without hiding execution evidence. |
| Diff, terminal, plan, files as side panels | **Adopt** | Keeps task context stable while inspecting work. |
| Provider adapters | **Adopt** | Codex/Claude/etc. must normalize into one domain model. |
| Direct client-to-environment networking | **Reject as default** | Conflicts with Telegram, unified history, NAT traversal, and centralized scheduling. |
| SSH as desktop launch helper | **Adapt as bootstrap only** | Useful for installation/recovery, but not the steady-state application protocol. |
| Electron desktop shell | **Reject** | Stackhour selected React + Tauri; keep native privileges narrow. |
| Electron browser preview implementation | **Reject** | Not portable to Tauri and too security-sensitive for the MVP. |
| Separate React Native app immediately | **Defer** | PWA reaches useful remote control much faster. |
| Very large orchestration components | **Reject** | Poor fit for a heavily agent-written application and parallel development. |

## Proposed Stackhour product model

### Topology

Stackhour should combine T3 Code's execution boundary with the existing
Stackhour coordinator/worker direction:

```text
Telegram ─┐
Web/PWA ──┼──── authenticated API / event stream ──── Stackhour hub
Tauri ────┘                                               │
                                                          │ durable tasks,
                                                          │ events, routing,
                                                          │ approvals, auth
                                  outbound node stream ────┼──── laptop node
                                  outbound node stream ────┼──── dev server
                                  outbound node stream ────└──── future nodes
```

The **hub** owns identity, task history, routing, client sessions,
notifications, and the durable event log. A **node** owns local checkouts,
worktrees, files, Git, terminals, and engine processes. Nodes dial out and
resume with a cursor; no inbound port is required on a laptop.

The existing filesystem/SSH pull worker is valuable migration material, but a
responsive multi-client UI needs a long-lived authenticated node protocol with
heartbeats, multiplexed events, cancellation, and reconnect/replay.

### Core entities

Keep the first domain deliberately small:

- `Node`: stable identity, label, platform, capabilities, connection status,
  software version, and last seen time.
- `Project`: logical repository identity plus one or more node-local
  checkouts.
- `Checkout`: node, absolute root, repository/branch state, and supported
  workspace modes.
- `Task`: durable user intent and lifecycle, independent of an engine process.
- `Run`: one attempt on one node with one engine/agent/model and access policy.
- `Event`: ordered durable task history.
- `Artifact`: image, log, patch, plan, or other large result referenced by an
  event.
- `Approval`: actionable request with scope, expiry, decision, and actor.
- `TerminalSession`: node-local PTY metadata and resumable output cursor.

Do not call every task a "chat." Telegram may look chat-like, but the domain is
a remotely executed task with a conversation timeline.

### Minimum event vocabulary

Start with stable product events rather than provider packets:

- `task.created`, `task.updated`, `task.archived`;
- `run.queued`, `run.started`, `run.interrupted`, `run.completed`,
  `run.failed`;
- `message.user`, `message.assistant.delta`, `message.assistant.completed`;
- `activity.started`, `activity.updated`, `activity.completed`;
- `approval.requested`, `approval.resolved`;
- `question.requested`, `question.answered`;
- `plan.proposed`, `plan.accepted`;
- `changes.updated`, `checkpoint.created`;
- `terminal.opened`, `terminal.output`, `terminal.exited`;
- `node.connected`, `node.disconnected`.

Store raw provider payloads separately or behind a versioned diagnostic field.
UI projections should not depend on them.

### UI MVP

The first React UI should contain:

1. **Node switcher/status:** laptop and remote server, online state, active
   work, version, and a clear offline explanation.
2. **Project/task sidebar:** grouped across nodes, with running/approval/error
   indicators and fast create/search.
3. **Task timeline:** user/assistant messages, grouped activity, pending
   actions, changed-file summaries, and reconnect-safe streaming.
4. **Composer:** text, images/files, node, checkout/worktree mode, engine/agent,
   model, Plan/Build, and access policy. Hide uncommon controls behind a
   compact menu.
5. **Approval/question cards:** first-class and usable from both PWA and
   Telegram.
6. **Diff panel:** changed files and a read-only patch first; inline review and
   checkpoint restore later.
7. **Terminal panel:** initially desktop/web only, with output replay and
   explicit write permission.
8. **Settings:** nodes, providers/engines, Telegram, access policies, and
   diagnostics.

Telegram should expose the same task/run/approval model with a narrower view,
not a separate job system. A task created in Telegram must open in the PWA or
desktop with its full history, and a graphical-client approval must resolve
the Telegram card too.

### Code boundaries for an agent-written app

Prefer explicit, narrow packages and generated boundaries:

```text
crates/
  stackhour-domain       IDs, commands, events, policies, projections
  stackhour-protocol     node/client wire schemas and version negotiation
  stackhour-hub          auth, durable log, routing, API, subscriptions
  stackhour-node         local capabilities and outbound connection
  stackhour-runtime      engine adapters and run supervision
  stackhour-git          checkout/worktree/checkpoint/diff operations
  stackhour-terminal     PTY lifecycle and resumable output
  stackhour-telegram     Telegram projection and commands

apps/
  web                    React UI and PWA
  desktop                thin Tauri shell around the shared web UI

packages/
  contracts-ts           generated from versioned Rust schemas
  client-runtime         connection, cache, commands, and projections
  ui                     reusable presentational components
```

Each UI feature should own its view, state adapter, commands, and focused tests:
`tasks`, `composer`, `timeline`, `approvals`, `diff`, `terminal`, `nodes`, and
`settings`. A route may compose features but should not become their state
machine. This matters more, not less, when agents write much of the code:
small ownership surfaces reduce accidental coupling and make tests useful as
executable context.

## Recommended sequence

### Phase 1 — domain and connectivity

- Introduce stable node/project/checkout/task/run/event IDs.
- Define versioned Rust commands/events and generated TypeScript contracts.
- Add the hub's durable ordered task event log and idempotent command IDs.
- Replace or supplement pull polling with an outbound authenticated node
  stream, reconnect cursors, capabilities, and heartbeats.
- Project the existing Telegram bridge through the new task model.

### Phase 2 — shared web/PWA

- Build the responsive shell, node/project/task navigation, timeline, composer,
  approvals, and connection diagnostics.
- Make streaming and reconnection correct before adding specialist panels.
- Add service-worker installation and offline read access; do not promise
  offline execution.

### Phase 3 — Tauri desktop

- Package the shared React application.
- Add only capabilities that need a trusted local shell: secure credential
  storage, notifications, local node bootstrap/control, file dialogs, and
  opening editors/external terminals.
- Keep domain state and networking in the shared client runtime.

### Phase 4 — development workbench

- Add Git status/branch actions, read-only diff, terminal, files, worktrees,
  per-turn checkpoints, and safe restore.
- Add source-control provider actions after local Git flows are reliable.
- Treat browser preview/automation as a separate security-reviewed feature.

### Phase 5 — native mobile only if evidence supports it

- Measure PWA limitations in daily use.
- Build React Native only for capabilities whose value exceeds maintaining a
  second UI implementation: reliable push/background behavior, share
  extensions, widgets/live activities, or high-performance terminal/review.

## Risks and open decisions

- **Hub placement:** the existing Linux coordinator is the natural first hub,
  but backup, upgrade, and recovery behavior must be defined before it becomes
  the only source of task history.
- **Node trust:** decide whether a paired node is fully trusted or receives
  per-project/per-capability grants.
- **Terminal security:** terminal read access, terminal input, and agent access
  are different permissions and should not collapse into one "full access"
  toggle.
- **Offline commands:** commands sent while a node sleeps need explicit
  queued/cancelled/expired semantics.
- **Worktree policy:** users need a visible choice among current checkout, new
  worktree, and possibly ephemeral clone.
- **Concurrent clients:** command IDs and optimistic UI must make repeated
  Telegram/PWA actions idempotent.
- **Event retention:** high-volume terminal and provider events should use
  chunked artifacts or a separate stream so the primary task log stays small.
- **Version skew:** hub, node, desktop, PWA, and Telegram may update at
  different times; capability negotiation is required from the first protocol
  version.

## Documentation drift observed in the reference

T3 Code's source is moving faster than some documentation:

- `docs/architecture/providers.md` says only Codex is implemented, while the
  reviewed source registers Codex, Claude, Cursor, Grok, and OpenCode drivers;
- the README lists four providers and omits the registered Grok driver;
- `docs/project/todo.md` still lists thread archiving while archiving exists in
  contracts and both desktop/web UI code.

This is not a criticism of an early, fast-moving project; it is a useful
warning for Stackhour. Maintain a generated capability matrix from the Rust
registry/protocol, and distinguish `implemented`, `tested`, `exposed`, and
`operationally proven`. The current Stackhour README/bridge documents already
show why those states must not be conflated.

## Reference source index

Key files at the reviewed T3 Code commit:

- [Architecture overview](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/docs/architecture/overview.md)
- [Remote architecture](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/docs/architecture/remote.md)
- [Connection runtime](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/docs/architecture/connection-runtime.md)
- [Orchestration contracts](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/packages/contracts/src/orchestration.ts)
- [Environment contracts](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/packages/contracts/src/environment.ts)
- [Shared client runtime](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/packages/client-runtime/README.md)
- [Web chat composition](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/web/src/components/ChatView.tsx)
- [Web timeline](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/web/src/components/chat/MessagesTimeline.tsx)
- [Web composer](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/web/src/components/chat/ChatComposer.tsx)
- [Web sidebar](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/web/src/components/Sidebar.tsx)
- [Web diff panel](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/web/src/components/DiffPanel.tsx)
- [Web terminal](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/web/src/components/ThreadTerminalDrawer.tsx)
- [Desktop SSH environment](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/desktop/src/ssh/DesktopSshEnvironment.ts)
- [Mobile thread detail](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/mobile/src/features/threads/ThreadDetailScreen.tsx)
- [Mobile review](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/mobile/src/features/review/ReviewSheet.tsx)
- [Mobile terminal](https://github.com/pingdotgg/t3code/blob/ece05087a70e94efcd57441337fa1249559362ba/apps/mobile/src/features/terminal/ThreadTerminalRouteScreen.tsx)
