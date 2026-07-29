//! The coordinator daemon (blocking; threads for timers).
//!
//! This is the module that makes the five bridge areas one program. It owns
//! nothing of its own beyond the wiring: the command surface plans
//! [`Action`]s, this executes them; the lanes run jobs, this feeds them; the
//! registry reloads, this ticks it.
//!
//! Startup: config load, dir creation, state load, media prune, setMyCommands,
//! the online banner, a 24h prune thread and a 1s mac-results thread. Then the
//! getUpdates long-poll loop, with the offset persisted per update and the
//! chat-id/bot gate applied before anything else looks at the message.
//!
//! Update routing priority is the JS's, and the order matters: voice beats
//! media beats text, because a voice note is also an audio attachment and a
//! captioned photo is also text.
//!
//! Every loop body is wrapped and logged. The daemon NEVER exits on error —
//! it announces itself on restart, so a crash loop would spam the chat.

use crate::config::{CoordinatorCfg, TargetCfg};
use crate::local_lane::{LaneContext, LocalJob, LocalLane, LocalTarget};
use crate::registry_ctx::RegistryCtx;
use crate::state::BridgeState;
use crate::telegram::Tg;
use crate::worker_lane::{WorkerContext, WorkerLane};
use crate::{log_line, BridgePaths};
use indexmap::IndexMap;
use serde_json::Value;
use stackhour_core::registry::{CommandKind, EngineDef, Registry, TargetSpec};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

// The scaffold this module started as declared its own `Coordinator` struct
// with a `local_q: VecDeque<QueuedPrompt>`, a `pending: HashMap<_,
// PendingJob>`, a `current: Option<RunningJob>` and a `busy` flag. Every one
// of those was built for real, and better, by the lane areas: the queue and
// the busy flag live behind ONE lock in `LocalLane` (splitting them
// reintroduces a TOCTOU the single-threaded JS cannot have), and the pending
// map lives in each `WorkerLane` beside the results poller that drains it. Keeping a
// second copy here would mean two answers to "is the bridge busy?". It is
// deleted rather than deprecated; `Runtime` below is the whole coordinator.

/// How often the mac worker's `results/` directory is swept.
const RESULTS_POLL: Duration = Duration::from_millis(1000);
/// How often stale media attachments are pruned.
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Attachments older than this are deleted by the prune timer.
const MEDIA_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// How long the loop pauses when getUpdates fails outright.
const POLL_BACKOFF: Duration = Duration::from_millis(1000);

// ---------------------------------------------------------------------------
// The shared context
// ---------------------------------------------------------------------------

/// Everything both lanes need, behind the traits they each declared.
///
/// One instance is shared (via `Arc`) by the local lane, the mac lane and the
/// update loop, so there is exactly ONE `BridgeState` and ONE registry
/// snapshot pointer in the process. The lanes run on their own threads, hence
/// the locks.
pub struct CoordCtx {
    cfg: CoordinatorCfg,
    pub(crate) paths: BridgePaths,
    /// The live registry. Swapped wholesale by `RegistryCtx::tick`; a job that
    /// already cloned what it needs is unaffected mid-run.
    reg: RwLock<Arc<Registry>>,
    state: Mutex<BridgeState>,
    log_path: PathBuf,
}

impl CoordCtx {
    pub fn new(cfg: CoordinatorCfg, paths: BridgePaths, reg: Arc<Registry>, state: BridgeState) -> CoordCtx {
        let log_path = paths.runtime_dir.join("coordinator.log");
        CoordCtx {
            cfg,
            paths,
            reg: RwLock::new(reg),
            state: Mutex::new(state),
            log_path,
        }
    }

    /// The current registry snapshot (a cheap Arc clone).
    pub fn registry(&self) -> Arc<Registry> {
        self.reg
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Install a freshly reloaded registry.
    pub fn set_registry(&self, next: Arc<Registry>) {
        if let Ok(mut g) = self.reg.write() {
            *g = next;
        }
    }

    /// Run `f` against the state under the lock.
    pub fn with_state<T>(&self, f: impl FnOnce(&mut BridgeState) -> T) -> T {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// A clone of the current state, for planning.
    pub fn state_snapshot(&self) -> BridgeState {
        self.with_state(|s| s.clone())
    }

    /// Persist state.json, logging a failure exactly as the JS does.
    pub fn save_state(&self) {
        let dir = self.paths.runtime_dir.clone();
        let log = |line: &str| self.log(line);
        self.with_state(|s| s.save_logged(&dir, &log));
    }

    pub fn cfg(&self) -> &CoordinatorCfg {
        &self.cfg
    }

    /// `targets[<name>].label` for every declared target — what `/where`
    /// names, and NOT limited to gcp/mac.
    pub fn target_labels(&self) -> IndexMap<String, String> {
        self.cfg
            .targets
            .iter()
            .map(|(name, target)| {
                let mut label = target.machine.as_ref().map_or_else(
                    || target.label.clone(),
                    |machine| format!("{machine} / {}", target.label),
                );
                if target.machine.is_some() {
                    if let Some(cwd) = &target.cwd {
                        label.push_str(&format!(" · {cwd}"));
                    }
                }
                (name.clone(), label)
            })
            .collect()
    }

    /// The per-engine binary and model for a target — the one place that
    /// knows `claudeBin`/`codexBin` and `model`/`codexModel` are engine-keyed
    /// pairs rather than a single field.
    fn engine_fields(t: &TargetCfg, engine: &str) -> (Option<String>, Option<String>) {
        if engine == "codex" {
            (t.codex_bin.clone(), t.codex_model.clone())
        } else {
            (t.claude_bin.clone(), t.model.clone())
        }
    }
}

impl LaneContext for CoordCtx {
    fn engine(&self, name: &str) -> Option<EngineDef> {
        self.registry().engines.get(name).cloned()
    }

    /// `targets[target]?.type === 'local' ? target : default` — a target that
    /// is not declared local is not this lane's to run.
    fn target(&self, name: &str, engine: &str) -> Option<LocalTarget> {
        let t = self.cfg.targets.get(name)?;
        if t.kind != "local" {
            return None;
        }
        let (bin, model) = CoordCtx::engine_fields(t, engine);
        Some(LocalTarget {
            name: name.to_string(),
            label: t.label.clone(),
            cwd: t.cwd.as_ref().map(PathBuf::from),
            bin,
            extra_path: t.extra_path.clone(),
            permission_mode: t.permission_mode.clone(),
            model,
        })
    }

    fn prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
        self.registry().prompts.render(name, vars)
    }

    fn control_keyboard(&self) -> Value {
        let reg = self.registry();
        self.with_state(|s| crate::keyboard::control_keyboard(s, &reg))
    }

    fn session(&self, target: &str, engine: &str, agent: Option<&str>) -> Option<String> {
        self.with_state(|s| s.session_for(target, engine, agent).map(str::to_string))
    }

    /// The JS `setSession` writes state.json inline; so does this, or a
    /// restart between two prompts silently loses the conversation.
    fn set_session(&self, target: &str, engine: &str, agent: Option<&str>, id: Option<String>) {
        self.with_state(|s| s.set_session_for(target, engine, agent, id));
        self.save_state();
    }

    fn log(&self, line: &str) {
        log_line(&self.log_path, line);
    }

    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn default_target(&self) -> String {
        self.cfg.default_target.clone()
    }
}

impl WorkerContext for CoordCtx {
    fn worker_label(&self, target: &str) -> String {
        self.cfg
            .targets
            .get(target)
            .map(|t| t.label.clone())
            .unwrap_or_else(|| target.to_string())
    }
}

/// The target roster the registry is loaded with, derived from the
/// coordinator config: WORKER (non-local) targets first in config file
/// order, then local targets in config file order.
///
/// The order is the shipped switch-command order (`builtin_commands_for`
/// splices the roster into the legacy /mac-before-/gcp slot), and
/// workers-first is what reproduces the Node coordinator's `setMyCommands`
/// order for a legacy gcp+mac config — mac (the worker) preceded gcp (the
/// local box) even though the config file listed gcp first.
pub fn target_specs(cfg: &CoordinatorCfg) -> Vec<TargetSpec> {
    let spec = |(name, target): (&String, &TargetCfg)| {
        let label = target.machine.as_ref().map_or_else(
            || target.label.clone(),
            |machine| format!("{machine} / {}", target.label),
        );
        TargetSpec::new(name.as_str(), label, target.kind.as_str()).with_icon(target.icon.clone())
    };
    cfg.targets
        .iter()
        .filter(|(_, t)| t.kind != "local")
        .map(spec)
        .chain(cfg.targets.iter().filter(|(_, t)| t.kind == "local").map(spec))
        .collect()
}

// ---------------------------------------------------------------------------
// The daemon
// ---------------------------------------------------------------------------

/// The wired-up runtime: transport, the local lane, one worker lane per
/// non-local target, the shared context.
pub struct Runtime {
    pub tg: Arc<Tg>,
    pub ctx: Arc<CoordCtx>,
    /// The local lane always exists; with zero local targets it simply never
    /// receives a directly-routed job (the unknown-target fallback may still
    /// pass through it, where its own default-target resolution applies).
    pub local: LocalLane,
    /// One lane per config target with `kind != "local"`, in config file
    /// order. Was a single hardcoded `mac: MacLane`.
    pub workers: IndexMap<String, WorkerLane>,
    pub registry: Mutex<RegistryCtx>,
}

impl Runtime {
    /// Assemble a runtime from an already-loaded config and registry.
    ///
    /// Split out from [`run_coordinator`] so tests can drive the whole update
    /// path against a mock Bot API without a daemon, a signal handler, or a
    /// real token.
    pub fn new(cfg: CoordinatorCfg, paths: BridgePaths, tg: Tg, registry: RegistryCtx) -> Runtime {
        let state = crate::state::load_with_defaults(
            &paths.runtime_dir,
            &cfg.default_target,
            &registry.registry().defaults.engine,
        );
        let ctx = Arc::new(CoordCtx::new(cfg, paths.clone(), registry.snapshot(), state));
        let tg = Arc::new(tg);
        let workers: IndexMap<String, WorkerLane> = ctx
            .cfg()
            .targets
            .iter()
            .filter(|(_, t)| t.kind != "local")
            .map(|(name, _)| {
                (
                    name.clone(),
                    WorkerLane::new(
                        tg.clone(),
                        ctx.clone() as Arc<dyn WorkerContext>,
                        paths.clone(),
                        name.clone(),
                    ),
                )
            })
            .collect();
        Runtime {
            local: LocalLane::new(tg.clone(), ctx.clone() as Arc<dyn LaneContext>),
            workers,
            registry: Mutex::new(registry),
            tg,
            ctx,
        }
    }

    fn log(&self, line: &str) {
        self.ctx.log(line);
    }

    /// The single `{{worker}}` online/offline flag the status templates
    /// render. CONTRACT DECISION: with several worker lanes it reads
    /// "online" only when EVERY lane's heartbeat is fresh (conservative — a
    /// down worker is worth noticing), and "offline" when there are no
    /// worker lanes at all (nothing is beating, same as the Node bridge with
    /// no heartbeat file). With the legacy single-mac config this is exactly
    /// the old `self.mac.worker_alive()`.
    fn workers_alive(&self) -> bool {
        !self.workers.is_empty() && self.workers.values().all(WorkerLane::worker_alive)
    }

    /// Build the planning environment from live state.
    fn env<'a>(
        &self,
        reg: &'a Registry,
        labels: &'a IndexMap<String, String>,
    ) -> crate::commands::CommandEnv<'a> {
        crate::commands::CommandEnv {
            reg,
            target_labels: labels,
            worker_alive: self.workers_alive(),
            busy: self.local.is_busy(),
            ship: self.ship_cfg(reg),
        }
    }

    /// `/ship`'s destination. Config-declared, with the registry defaults as
    /// the fallback — the JS hardcoded two literals here.
    fn ship_cfg(&self, reg: &Registry) -> crate::commands::ShipCfg {
        let mut ship = crate::commands::ShipCfg {
            target: self.ctx.cfg().default_target.clone(),
            engine: reg.defaults.engine.clone(),
        };
        if let Some(v) = self.ctx.cfg().raw.get("ship") {
            if let Some(t) = v.get("target").and_then(Value::as_str) {
                ship.target = t.to_string();
            }
            if let Some(e) = v.get("engine").and_then(Value::as_str) {
                ship.engine = e.to_string();
            }
        }
        ship
    }

    // -- the Action interpreter --------------------------------------------

    /// Execute one planned [`Action`]. This is the ONLY place in the bridge
    /// that turns a plan into an effect.
    fn apply(&self, action: crate::commands::Action) {
        use crate::commands::Action;
        match action {
            Action::Send { text, html, keyboard } => {
                let extra = keyboard.map(|kb| serde_json::json!({ "reply_markup": kb }));
                self.tg
                    .send_message(&text, html.then_some("HTML"), extra.as_ref());
            }
            Action::SaveState => self.ctx.save_state(),
            Action::RoutePrompt { text, message_id } => self.route_prompt(&text, message_id, None),
            Action::Stop => self.stop(),
            Action::RunCommand { command, raw } => self.run_command(&command, &raw),
            Action::Confirm { command, raw } => {
                let kb = crate::keyboard::confirm_keyboard(&command, &raw);
                let text = self
                    .ctx
                    .prompt("confirm", &[("command", &command), ("args", &raw)]);
                self.tg
                    .send_message(&text, None, Some(&serde_json::json!({ "reply_markup": kb })));
            }
            Action::AnswerCallback { id, text } => {
                self.tg.answer_cb_text(&id, text.as_deref());
            }
            Action::RefreshKeyboard { message_id, text } => {
                let kb = self.ctx.control_keyboard();
                self.tg
                    .edit(message_id, &text, Some(serde_json::json!({ "reply_markup": kb })));
            }
        }
    }

    fn apply_all(&self, actions: Vec<crate::commands::Action>) {
        for action in actions {
            self.apply(action);
        }
    }

    /// `routePrompt` — react, then hand to whichever lane owns the active
    /// target. The engine and agent are captured HERE, so a switch that
    /// arrives while the job waits does not retarget it.
    ///
    /// Routing, generalized from the JS's two-way local/mac split:
    /// * a `type == "local"` target → the local lane;
    /// * a target with its own worker lane → that lane;
    /// * anything else (a `ship` target absent from config, a stale
    ///   state.json) → the DEFAULT target's lane, mirroring the local
    ///   lane's own unknown-target fallback. Never a panic.
    fn route_prompt(&self, text: &str, message_id: Option<i64>, media: Option<Value>) {
        if let Some(id) = message_id {
            self.tg.react_eyes(id);
        }
        let (target, engine, agent) = self
            .ctx
            .with_state(|s| (s.active.clone(), s.engine.clone(), s.agent.clone()));

        let is_local = self
            .ctx
            .cfg()
            .targets
            .get(&target)
            .map(|t| t.kind == "local")
            .unwrap_or(false);

        if is_local {
            // The local lane reads the media path out of the prompt text,
            // which the media handler has already rewritten.
            self.local.enqueue(LocalJob {
                prompt: text.to_string(),
                engine,
                target,
                agent,
            });
        } else if let Some(lane) = self.workers.get(&target) {
            lane.dispatch(text, &engine, agent.as_deref(), media);
        } else if let Some(lane) = self.workers.get(&self.ctx.default_target()) {
            // No lane for this target and it is not local: the default
            // target's worker lane takes it (leader-only fallback).
            lane.dispatch(text, &engine, agent.as_deref(), media);
        } else {
            // The default target is local (or itself misconfigured): the
            // local lane's own fallback resolves — or logs — it, exactly as
            // it does for an unknown target today.
            self.local.enqueue(LocalJob {
                prompt: text.to_string(),
                engine,
                target,
                agent,
            });
        }
    }

    /// `/stop`: kill the local child AND sweep every worker lane's queued
    /// jobs, then report what actually happened.
    fn stop(&self) {
        let stopped_local = self.local.stop_current();
        let mut cancelled = crate::worker_lane::CancelOutcome::default();
        for lane in self.workers.values() {
            let out = lane.cancel_queued();
            cancelled.cancelled += out.cancelled;
            cancelled.running += out.running;
        }
        let reg = self.ctx.registry();
        let labels = self.ctx.target_labels();
        let env = self.env(&reg, &labels);
        let text = crate::commands::stop_text(
            &env,
            crate::commands::StopOutcome {
                stopped_local,
                cancelled: cancelled.cancelled,
                running: cancelled.running,
            },
        );
        self.tg.send_message(&text, None, None);
    }

    // -- declarative commands ----------------------------------------------

    /// Run a config-declared command: agent switch, prompt, skill, shell or
    /// sequence. This is the path that makes `/deploy` in a TOML file a real
    /// Telegram command.
    fn run_command(&self, command: &str, raw: &str) {
        let reg = self.ctx.registry();
        let Some(def) = crate::commands::table(&reg).get(command).cloned() else {
            let labels = self.ctx.target_labels();
            let env = self.env(&reg, &labels);
            self.apply(crate::commands::Action::Send {
                text: crate::commands::unknown_text(&env),
                html: true,
                keyboard: None,
            });
            return;
        };

        // Any kind may carry an `agent` pre-switch.
        if def.kind != CommandKind::Agent {
            if let Some(agent) = def.agent.as_deref() {
                self.ctx.with_state(|s| s.agent = Some(agent.to_string()));
                self.ctx.save_state();
            }
        }

        match def.kind {
            CommandKind::Agent => {
                // `/agent <name>` takes its target from the argument; a
                // command declared `kind = "agent"` takes it from `agent`.
                let arg = def.agent.unwrap_or_else(|| raw.trim().to_string());
                let (text, ok) = {
                    let result = self.ctx.with_state(|s| crate::souls::select_agent(s, &reg, &arg));
                    match result {
                        Ok(t) => (t, true),
                        Err(t) => (t, false),
                    }
                };
                if ok {
                    self.ctx.save_state();
                }
                self.apply(crate::commands::Action::Send {
                    text,
                    html: true,
                    keyboard: Some(self.ctx.control_keyboard()),
                });
            }
            CommandKind::Prompt => {
                let template = def.template.as_deref().unwrap_or(command);
                let text = reg.prompts.render(template, &[("args", raw)]);
                self.route_prompt(&text, None, None);
            }
            CommandKind::Skill => {
                let skill = def.skill.as_deref().unwrap_or(command);
                match crate::skills::plan_invocation(&reg, skill, raw) {
                    Ok(plan) => {
                        if let Some(agent) = plan.agent_override.clone() {
                            self.ctx.with_state(|s| s.agent = Some(agent));
                            self.ctx.save_state();
                        }
                        self.route_prompt(&plan.user_prompt, None, None);
                    }
                    Err(e) => {
                        self.apply(crate::commands::Action::Send {
                            text: e,
                            html: true,
                            keyboard: None,
                        });
                    }
                }
            }
            CommandKind::Shell => self.run_shell(&def, raw),
            CommandKind::Sequence => {
                // Abort on the first failure, as declared.
                for step in &def.steps {
                    self.run_command(step, raw);
                }
            }
            // Engine/Target/Builtin never reach here — the planner handled
            // them and returned their Send actions directly.
            _ => {}
        }
    }

    /// `kind = "shell"`: run the FIXED argv and reply with its output. The
    /// argv is fixed by the config precisely so a Telegram message can never
    /// choose the program; `raw` is appended as ONE argument, never split.
    fn run_shell(&self, def: &stackhour_core::registry::CommandDef, raw: &str) {
        let Some(argv) = def.argv.as_ref().filter(|a| !a.is_empty()) else {
            return;
        };
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if !raw.trim().is_empty() {
            cmd.arg(raw);
        }
        let out = match cmd.output() {
            Ok(out) => out,
            Err(e) => {
                let text = self.ctx.prompt("error-run", &[("error", &e.to_string())]);
                self.tg.send_message(&text, None, None);
                return;
            }
        };
        let body = String::from_utf8_lossy(if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        });
        let text = body.trim();
        let text = if text.is_empty() {
            self.ctx.prompt("no-output", &[])
        } else {
            text.to_string()
        };
        crate::local_lane::deliver_final(&self.tg, self.ctx.as_ref(), &text, None);
    }

    // -- update routing ----------------------------------------------------

    /// Route one update. Returns after the update has been fully handled (the
    /// lanes are what run asynchronously, not this).
    ///
    /// The chat-id and is_bot gates are applied here, before any handler sees
    /// the message: a bridge that answers a stranger is a bridge that leaks
    /// the owner's shell.
    pub fn handle_update(&self, u: &Value) {
        // Reload the registry before dispatching, so an edited config takes
        // effect on the very next message.
        if let Ok(mut reg) = self.registry.lock() {
            if reg.tick() {
                for err in reg.new_errors() {
                    self.log(&format!("registry: {err}"));
                }
                self.ctx.set_registry(reg.snapshot());
            }
        }

        let chat_id = self.tg.chat_id();

        if let Some(cb) = u.get("callback_query").filter(|v| v.is_object()) {
            let from_chat = cb.pointer("/message/chat/id").and_then(Value::as_i64);
            if from_chat == Some(chat_id) {
                self.handle_callback(cb);
            }
            return;
        }

        let Some(m) = u
            .get("message")
            .or_else(|| u.get("edited_message"))
            .filter(|v| v.is_object())
        else {
            return;
        };
        match m.pointer("/chat/id").and_then(Value::as_i64) {
            Some(id) if id == chat_id => {}
            other => {
                self.log(&format!("ignoring chat {}", other.unwrap_or_default()));
                return;
            }
        }
        if m.pointer("/from/is_bot").and_then(Value::as_bool) == Some(true) {
            return;
        }

        let message_id = m.get("message_id").and_then(Value::as_i64).unwrap_or_default();

        // Priority is load-bearing: a voice note is also an audio
        // attachment, and a captioned photo also carries text.
        let voice = crate::media::extract_voice(m);
        let att = crate::media::extract_attachment(m);
        if voice.is_some() || att.is_some() {
            // The registry Arc is held for the whole handler so MediaCtx can
            // borrow its prompt store.
            let reg = self.ctx.registry();
            let eleven = crate::media::ElevenLabs::from_cfg(self.ctx.cfg());
            let log = |line: &str| self.ctx.log(line);
            let mctx = crate::media::MediaCtx {
                prompts: &reg.prompts,
                media_dir: &self.ctx.paths.media_dir,
                max_bytes: self.ctx.cfg().max_media_bytes,
                eleven: &eleven,
                log: &log,
            };
            if let Some(voice) = voice {
                if let Some(transcript) =
                    crate::media::handle_voice_message(self.tg.as_ref(), &mctx, message_id, &voice)
                {
                    // Routed as plain text with msgId = null: the 👀 reaction
                    // already happened, and a second one would be noise.
                    self.route_prompt(&transcript, None, None);
                }
            } else if let Some(att) = att {
                let caption = m.get("caption").and_then(Value::as_str).unwrap_or_default();
                if let Some((caption, media)) =
                    crate::media::handle_media_message(self.tg.as_ref(), &mctx, message_id, caption, &att)
                {
                    let prompt = crate::media::media_prompt(&reg.prompts, &caption, &media);
                    self.route_prompt(&prompt, None, None);
                }
            }
            return;
        }

        let Some(text) = m.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
            return;
        };
        self.log(&format!("msg: {}", text.chars().take(80).collect::<String>()));
        self.handle_text(text, Some(message_id));
    }

    /// Plan and execute one text message.
    pub fn handle_text(&self, text: &str, message_id: Option<i64>) {
        let reg = self.ctx.registry();
        let labels = self.ctx.target_labels();
        let env = self.env(&reg, &labels);
        let actions = self
            .ctx
            .with_state(|s| crate::commands::plan_text(&env, s, text, message_id));
        self.apply_all(actions);
    }

    /// Plan and execute one callback query.
    pub fn handle_callback(&self, cb: &Value) {
        let reg = self.ctx.registry();
        let labels = self.ctx.target_labels();
        let env = self.env(&reg, &labels);
        let actions = self
            .ctx
            .with_state(|s| crate::commands::plan_callback(&env, s, cb));
        self.apply_all(actions);
    }

    /// One sweep of the shared results directory, across every worker lane.
    ///
    /// Each lane takes the results matched to its own pending ids; the FIRST
    /// lane additionally sweeps orphan results (no pending entry anywhere —
    /// coordinator restart), so an orphan is delivered exactly once and a
    /// sibling lane's matched result is never mislabelled. With the legacy
    /// single-mac config this is exactly the old `self.mac.poll_results()`.
    pub fn poll_results(&self) -> usize {
        let mut lanes = self.workers.values();
        let Some(first) = lanes.next() else {
            return 0; // leader-only with zero workers dispatches nothing
        };
        let foreign: std::collections::HashSet<String> =
            lanes.clone().flat_map(WorkerLane::pending_ids).collect();
        let mut delivered = first.poll_with_orphans(&foreign);
        for lane in lanes {
            delivered += lane.poll_matched();
        }
        delivered
    }
}

/// Run the coordinator daemon forever.
pub fn run_coordinator(runtime_dir: &Path) -> ! {
    let paths = BridgePaths::from_runtime_dir(runtime_dir);
    let _ = paths.ensure_dirs();
    let log_path = paths.runtime_dir.join("coordinator.log");

    let cfg = match crate::config::load_coordinator_cfg(&paths.config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            log_line(&log_path, &format!("coordinator config error: {e}"));
            std::process::exit(1);
        }
    };

    // The registry is loaded with the CONFIG's target roster: the generated
    // switch commands, target validation and /help lines all follow the
    // deployment's actual targets. Workers-first ordering (see
    // [`target_specs`]) reproduces the legacy /mac-before-/gcp surface for a
    // legacy config. Hot reloads replay the same roster — `RegistryCtx::tick`
    // rebuilds via `Registry::rebuild`.
    let storage = stackhour_core::paths::resolve_storage_paths_from_process_env();
    let registry = RegistryCtx::new_with_targets(&storage, &target_specs(&cfg), &cfg.default_target);
    for err in registry.new_errors() {
        log_line(&log_path, &format!("registry: {err}"));
    }

    let mut tg_cfg =
        crate::telegram::TgConfig::new(cfg.token.clone(), cfg.chat_id).with_log_path(Some(log_path));
    if let Some(root) = &cfg.api_root {
        tg_cfg = tg_cfg.with_api_root(root.clone());
    }
    let tg = Tg::with_config(tg_cfg);
    let rt = Arc::new(Runtime::new(cfg, paths, tg, registry));

    serve(rt)
}

/// What the daemon hands `Tg::set_my_commands` at startup.
///
/// `set_my_commands` adds the `{ "commands": … }` wrapper itself, so this must
/// be the LIST. It used to be [`crate::commands::my_commands_payload`], which
/// is already wrapped: the body went out as `{"commands":{"commands":[…]}}`,
/// Telegram answered 400, `call()` swallowed it, and the bot ran with NO
/// registered commands and nothing in the log. Every unit test passed
/// throughout, because none of them exercised this line — it took driving the
/// real binary against a mock Bot API to see it
/// (`test/parity/command-surface.mjs`).
fn registration_payload(reg: &Registry) -> Value {
    crate::commands::my_commands_list(reg)
}

/// The startup sequence and the poll loop, given an assembled runtime.
fn serve(rt: Arc<Runtime>) -> ! {
    let reg = rt.ctx.registry();
    let labels = rt.ctx.target_labels();

    {
        let (engine, target) = rt.ctx.with_state(|s| (s.engine.clone(), s.active.clone()));
        rt.log(&format!(
            "coordinator online — {} on {}",
            crate::commands::engine_label(&reg, &engine),
            crate::commands::target_label(&labels, &target)
        ));
    }

    crate::media::prune_media(&rt.ctx.paths.media_dir, MEDIA_MAX_AGE);
    rt.tg.set_my_commands(registration_payload(&reg));

    // The online banner. Sent before the loop starts, so a restart is visible.
    {
        let env = rt.env(&reg, &labels);
        let text = rt.ctx.with_state(|s| crate::commands::online_text(&env, s));
        let kb = rt.ctx.control_keyboard();
        rt.tg
            .send_message(&text, None, Some(&serde_json::json!({ "reply_markup": kb })));
    }

    // Timer threads. Both are detached and both swallow their own errors —
    // a failed sweep must not take the daemon with it.
    {
        let rt = rt.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(PRUNE_EVERY);
            crate::media::prune_media(&rt.ctx.paths.media_dir, MEDIA_MAX_AGE);
        });
    }
    {
        let rt = rt.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(RESULTS_POLL);
            rt.poll_results();
        });
    }

    loop {
        let offset = rt.ctx.with_state(|s| s.offset);
        let Some(updates) = rt.tg.get_updates(offset) else {
            std::thread::sleep(POLL_BACKOFF);
            continue;
        };
        let Some(list) = updates.as_array() else {
            std::thread::sleep(rt.tg.empty_poll_sleep());
            continue;
        };
        for u in list {
            // The offset advances and is persisted BEFORE the update is
            // handled. A handler that panics must not replay the message that
            // caused it on every restart forever.
            if let Some(id) = u.get("update_id").and_then(Value::as_i64) {
                rt.ctx.with_state(|s| s.offset = id + 1);
                rt.ctx.save_state();
            }
            rt.handle_update(u);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The roster ordering rule: workers first (config order), then locals
    /// (config order). For the legacy config — gcp declared first but LOCAL,
    /// mac second but a worker — this yields mac-before-gcp, which is what
    /// keeps the generated switch commands (and therefore the registration
    /// payload below) in the Node coordinator's `setMyCommands` order.
    #[test]
    fn target_specs_orders_workers_first_then_locals_in_config_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "token": "t", "chatId": 1, "defaultTarget": "gcp",
                "targets": {
                    "gcp": { "type": "local", "label": "GCP" },
                    "pi": { "label": "Pi" },
                    "attic": { "type": "local", "label": "Attic" },
                    "mac": { "label": "Mac" },
                },
            })
            .to_string(),
        )
        .unwrap();
        let cfg = crate::config::load_coordinator_cfg(&path).unwrap();
        let specs = target_specs(&cfg);
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["pi", "mac", "gcp", "attic"],
            "workers in config order, then locals in config order"
        );
        assert!(specs[2].is_local() && specs[3].is_local());
        assert_eq!(specs[0].label, "Pi");
    }

    /// The daemon's registration body must be the command ARRAY. If this ever
    /// becomes an object again, the bot registers nothing and says nothing.
    #[test]
    fn the_registration_payload_is_a_bare_command_array() {
        let reg = stackhour_core::registry::load(Path::new("/nonexistent/stackhour-registration"));
        let payload = registration_payload(&reg);
        let list = payload
            .as_array()
            .expect("setMyCommands takes the list, not the wrapped body");
        let names: Vec<&str> = list.iter().map(|c| c["command"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["claude", "codex", "mac", "gcp", "ship", "where", "new", "stop", "menu", "help"]
        );
    }
}
