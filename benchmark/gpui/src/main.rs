use benchmark_core::{LanguageServer, Symbol, descendants, process_memory, scan_project};
use gpui::{
    App, Application, Bounds, Context, HighlightStyle, SharedString, StyledText, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, point, prelude::*, px, rgb, size,
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::Instant;

#[derive(Clone)]
struct Logger {
    started: Instant,
    phase: String,
    run_id: String,
    path: Option<PathBuf>,
}

impl Logger {
    fn emit(&self, row: Value) {
        let mut object = Map::new();
        object.insert("schemaVersion".into(), json!(1));
        object.insert("candidate".into(), json!("gpui"));
        object.insert("phase".into(), json!(self.phase));
        object.insert("runId".into(), json!(self.run_id));
        object.insert(
            "timestampNs".into(),
            json!(self.started.elapsed().as_nanos() as u64),
        );
        if let Some(values) = row.as_object() {
            object.extend(values.clone());
        }
        let line = serde_json::to_string(&object).unwrap();
        if let Some(path) = &self.path {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        } else {
            println!("{line}");
        }
    }

    fn emit_frames(&self, frames: &[f64]) {
        if let Some(path) = &self.path {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                for duration in frames {
                    let mut object = Map::new();
                    object.insert("schemaVersion".into(), json!(1));
                    object.insert("candidate".into(), json!("gpui"));
                    object.insert("phase".into(), json!(self.phase));
                    object.insert("runId".into(), json!(self.run_id));
                    object.insert(
                        "timestampNs".into(),
                        json!(self.started.elapsed().as_nanos() as u64),
                    );
                    object.insert("event".into(), json!("frame"));
                    object.insert("durationMs".into(), json!(duration));
                    object.insert("workload".into(), json!("scroll"));
                    let _ = writeln!(file, "{}", serde_json::to_string(&object).unwrap());
                }
            }
        } else {
            for duration in frames {
                self.emit(json!({ "event": "frame", "durationMs": duration, "workload": "scroll" }));
            }
        }
    }

    fn memory(&self, label: &str) {
        self.emit(json!({ "event": "memory_sample", "label": label, "processes": process_memory(&descendants(std::process::id())) }));
    }
}

enum BackendCommand {
    Scan,
    Read(String),
    Symbols(String),
}
enum BackendMessage {
    Scan(Vec<String>),
    Read(String, String),
    Symbols(Vec<Symbol>),
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Initial,
    WaitingFirst,
    WaitingScan,
    TreeNeedsFrame,
    TreeScheduled,
    WaitingRead,
    TextNeedsFrame,
    TextScheduled,
    WaitingSymbols,
    OutlineNeedsFrame,
    OutlineScheduled,
    Scrolling,
    ScrollFinal,
    Complete,
}

struct BenchView {
    logger: Logger,
    commands: mpsc::Sender<BackendCommand>,
    messages: mpsc::Receiver<BackendMessage>,
    autorun: bool,
    stage: Stage,
    files: Vec<String>,
    lines: Vec<String>,
    symbols: Vec<Symbol>,
    selected: String,
    tree_top: f32,
    editor_top: f32,
    current_read_is_switch: bool,
    switch_index: usize,
    scroll_step: usize,
    last_frame: Instant,
    frame_times: Vec<f64>,
}

impl BenchView {
    fn new(
        _window: &mut Window,
        cx: &mut Context<Self>,
        logger: Logger,
        fixture: PathBuf,
        lsp_command: PathBuf,
        autorun: bool,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let worker_logger = logger.clone();
        thread::spawn(move || {
            let mut lsp: Option<LanguageServer> = None;
            while let Ok(command) = command_rx.recv() {
                let result = match command {
                    BackendCommand::Scan => match scan_project(&fixture) {
                        Ok(files) => {
                            worker_logger
                                .emit(json!({ "event": "project_scan_complete", "count": files.len() }));
                            BackendMessage::Scan(files)
                        }
                        Err(error) => BackendMessage::Error(error.to_string()),
                    },
                    BackendCommand::Read(relative) => match fs::read_to_string(fixture.join(&relative)) {
                        Ok(text) => {
                            worker_logger.emit(json!({ "event": "disk_read_complete", "path": relative, "bytes": text.len() }));
                            BackendMessage::Read(relative, text)
                        }
                        Err(error) => BackendMessage::Error(error.to_string()),
                    },
                    BackendCommand::Symbols(relative) => {
                        if lsp.is_none() {
                            match LanguageServer::start(&lsp_command, &fixture) {
                                Ok(server) => lsp = Some(server),
                                Err(error) => {
                                    let _ = message_tx.send(BackendMessage::Error(error));
                                    continue;
                                }
                            }
                        }
                        match lsp.as_mut().unwrap().document_symbols(&relative) {
                            Ok(symbols) => BackendMessage::Symbols(symbols),
                            Err(error) => BackendMessage::Error(error),
                        }
                    }
                };
                if message_tx.send(result).is_err() {
                    break;
                }
            }
        });
        cx.notify();
        Self {
            logger,
            commands: command_tx,
            messages: message_rx,
            autorun,
            stage: Stage::Initial,
            files: Vec::new(),
            lines: Vec::new(),
            symbols: Vec::new(),
            selected: "src/selected.ts".into(),
            tree_top: 0.0,
            editor_top: 0.0,
            current_read_is_switch: false,
            switch_index: 0,
            scroll_step: 0,
            last_frame: Instant::now(),
            frame_times: Vec::with_capacity(640),
        }
    }

    fn open_file(&mut self, path: String, is_switch: bool) {
        self.selected = path.clone();
        self.current_read_is_switch = is_switch;
        self.logger.emit(json!({ "event": "file_click", "path": path }));
        let _ = self.commands.send(BackendCommand::Read(self.selected.clone()));
        self.stage = Stage::WaitingRead;
    }

    fn handle_messages(&mut self) {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                BackendMessage::Scan(files) => {
                    self.files = files;
                    self.stage = Stage::TreeNeedsFrame;
                }
                BackendMessage::Read(path, text) => {
                    self.selected = path;
                    self.lines = text.lines().map(str::to_owned).collect();
                    self.editor_top = 0.0;
                    self.stage = Stage::TextNeedsFrame;
                }
                BackendMessage::Symbols(symbols) => {
                    self.logger
                        .emit(json!({ "event": "lsp_response", "count": symbols.len() }));
                    self.symbols = symbols;
                    self.stage = Stage::OutlineNeedsFrame;
                }
                BackendMessage::Error(error) => {
                    self.logger
                        .emit(json!({ "event": "benchmark_error", "message": error }));
                    self.stage = Stage::Complete;
                }
            }
        }
    }

    fn schedule_transitions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.stage {
            Stage::Initial => {
                self.stage = Stage::WaitingFirst;
                let logger = self.logger.clone();
                cx.on_next_frame(window, move |this, _, cx| {
                    logger.emit(json!({ "event": "first_frame" }));
                    logger.memory("idle");
                    logger.emit(json!({ "event": "project_open_requested" }));
                    let _ = this.commands.send(BackendCommand::Scan);
                    this.stage = Stage::WaitingScan;
                    cx.notify();
                });
            }
            Stage::TreeNeedsFrame => {
                self.stage = Stage::TreeScheduled;
                let logger = self.logger.clone();
                cx.on_next_frame(window, move |this, _, cx| {
                    logger.emit(json!({ "event": "project_tree_visible" }));
                    logger.emit(json!({ "event": "tree_presented" }));
                    this.open_file("src/selected.ts".into(), false);
                    cx.notify();
                });
            }
            Stage::TextNeedsFrame => {
                self.stage = Stage::TextScheduled;
                let logger = self.logger.clone();
                let is_switch = self.current_read_is_switch;
                let path = self.selected.clone();
                cx.on_next_frame(window, move |_this, window, cx| {
                    logger.emit(json!({ "event": "text_presented", "path": path }));
                    cx.on_next_frame(window, move |this, _, cx| {
                        logger.emit(json!({ "event": "stable_frame", "path": this.selected }));
                        if is_switch {
                            this.switch_index += 1;
                            if this.switch_index < 30 {
                                let path = if this.switch_index % 2 == 0 {
                                    "src/alternate.ts"
                                } else {
                                    "src/selected.ts"
                                };
                                this.open_file(path.into(), true);
                            } else {
                                logger.emit(json!({ "event": "benchmark_complete" }));
                                logger.memory("complete");
                                this.stage = Stage::Complete;
                                cx.quit();
                            }
                        } else {
                            logger.emit(
                                json!({ "event": "lsp_request", "method": "textDocument/documentSymbol" }),
                            );
                            let _ = this
                                .commands
                                .send(BackendCommand::Symbols("src/selected.ts".into()));
                            this.stage = Stage::WaitingSymbols;
                        }
                        cx.notify();
                    });
                    cx.notify();
                });
            }
            Stage::OutlineNeedsFrame => {
                self.stage = Stage::OutlineScheduled;
                let logger = self.logger.clone();
                cx.on_next_frame(window, move |this, _, cx| {
                    logger.emit(json!({ "event": "outline_presented" }));
                    logger.memory("loaded");
                    if this.autorun {
                        this.stage = Stage::Scrolling;
                        this.scroll_step = 0;
                        this.last_frame = Instant::now();
                    } else {
                        this.stage = Stage::Complete;
                    }
                    cx.notify();
                });
            }
            _ => {}
        }
    }

    fn advance_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::Scrolling {
            return;
        }
        let now = Instant::now();
        self.frame_times
            .push((now - self.last_frame).as_secs_f64() * 1000.0);
        self.last_frame = now;
        let leg = self.scroll_step / 160;
        let step = self.scroll_step % 160;
        let t = step as f32 / 159.0;
        let eased = (1.0 - (std::f32::consts::PI * t).cos()) / 2.0;
        let value = if leg % 2 == 0 { eased } else { 1.0 - eased };
        self.editor_top = value * 19_970.0;
        self.tree_top = value * 5_080.0;
        self.scroll_step += 1;
        if self.scroll_step < 640 {
            window.request_animation_frame();
        } else {
            self.stage = Stage::ScrollFinal;
            let logger = self.logger.clone();
            cx.on_next_frame(window, move |this, _, cx| { logger.emit_frames(&this.frame_times); let worst = this.frame_times.iter().copied().fold(0.0_f64, f64::max); logger.emit(json!({ "event": "main_thread_stall", "durationMs": (worst - 16.7).max(0.0), "workload": "scroll" })); this.switch_index = 0; this.open_file("src/alternate.ts".into(), true); cx.notify(); });
        }
    }

    fn tree_rows(&self) -> impl Iterator<Item = (usize, &str)> {
        let first = (self.tree_top.floor() as usize).saturating_sub(4);
        let last = (self.tree_top.floor() as usize + 44).min(self.files.len());
        self.files[first..last]
            .iter()
            .enumerate()
            .map(move |(offset, path)| (first + offset, path.as_str()))
    }

    fn editor_rows(&self) -> impl Iterator<Item = (usize, &str)> {
        let first = (self.editor_top.floor() as usize).saturating_sub(4);
        let last = (self.editor_top.floor() as usize + 34).min(self.lines.len());
        self.lines[first..last]
            .iter()
            .enumerate()
            .map(move |(offset, line)| (first + offset, line.as_str()))
    }
}

impl Render for BenchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.handle_messages();
        self.schedule_transitions(window, cx);
        self.advance_scroll(window, cx);
        if matches!(
            self.stage,
            Stage::WaitingScan | Stage::WaitingRead | Stage::WaitingSymbols
        ) {
            window.request_animation_frame();
        }

        let tree_first = (self.tree_top.floor() as usize).saturating_sub(4);
        let editor_first = (self.editor_top.floor() as usize).saturating_sub(4);
        let tree = self.tree_rows().map(|(_, path)| {
            div()
                .h(px(18.))
                .flex()
                .items_center()
                .px(px(12.))
                .bg(if path == self.selected {
                    rgb(0x2c3240)
                } else {
                    rgb(0x1c1f26)
                })
                .text_color(rgb(0xb7bdc9))
                .child(format!("◇ {}", path.rsplit('/').next().unwrap_or(path)))
        });
        let editor = self.editor_rows().map(|(row, line)| {
            div()
                .h(px(20.))
                .flex()
                .items_center()
                .child(
                    div()
                        .w(px(58.))
                        .pr(px(14.))
                        .text_right()
                        .text_color(rgb(0x565d6c))
                        .child(format!("{}", row + 1)),
                )
                .child(highlighted(line))
        });
        let outline = self.symbols.iter().take(24).map(|symbol| {
            div()
                .h(px(22.))
                .flex()
                .items_center()
                .child(div().text_color(rgb(0xc792ea)).mr(px(7.)).child("◇"))
                .child(symbol.name.clone())
        });

        let activity = div()
            .w(px(48.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(10.))
            .gap(px(8.))
            .bg(rgb(0x1d2027))
            .border_r_1()
            .border_color(rgb(0x30343e))
            .child(
                div()
                    .w(px(32.))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x292d36))
                    .text_color(rgb(0xf3f5f8))
                    .child("⌘"),
            )
            .child(div().child("⌕"))
            .child(div().child("⑂"))
            .child(div().child("△"));
        let explorer = div()
            .w(px(260.))
            .h_full()
            .bg(rgb(0x1c1f26))
            .border_r_1()
            .border_color(rgb(0x30343e))
            .child(section_title("EXPLORER · CONTROLLED FIXTURE"))
            .child(
                div()
                    .h(px(720.))
                    .overflow_hidden()
                    .font_family("Noto Sans Mono")
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .mt(px((tree_first as f32 - self.tree_top) * 18.))
                            .children(tree),
                    ),
            );
        let tabs = div()
            .h(px(36.))
            .flex()
            .bg(rgb(0x1b1e24))
            .border_b_1()
            .border_color(rgb(0x30343e))
            .child(tab("selected.ts", true))
            .child(tab("alternate.ts", false));
        let editor_view = div()
            .h(px(600.))
            .overflow_hidden()
            .bg(rgb(0x17191f))
            .font_family("Noto Sans Mono")
            .line_height(px(20.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .mt(px((editor_first as f32 - self.editor_top) * 20.))
                    .children(editor),
            );
        let output = div()
            .h(px(140.))
            .border_t_1()
            .border_color(rgb(0x30343e))
            .bg(rgb(0x181a20))
            .child(
                div()
                    .h(px(31.))
                    .flex()
                    .items_center()
                    .gap(px(20.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(rgb(0x282c35))
                    .child("OUTPUT")
                    .child("PROBLEMS")
                    .child("TERMINAL"),
            )
            .child(
                div()
                    .p(px(8.))
                    .font_family("Noto Sans Mono")
                    .text_color(rgb(0x8f97a6))
                    .child("[benchmark] deterministic project fixture\n")
                    .child(format!("[scanner] {} TypeScript files\n", self.files.len()))
                    .child(format!("[typescript] {} document symbols", self.symbols.len())),
            );
        let editor_panel = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .child(editor_view)
            .child(output);
        let outline_panel = div()
            .w(px(220.))
            .h_full()
            .border_l_1()
            .border_color(rgb(0x30343e))
            .bg(rgb(0x1b1e24))
            .child(section_title("OUTLINE"))
            .child(
                div()
                    .p(px(8.))
                    .font_family("Noto Sans Mono")
                    .text_size(px(12.))
                    .text_color(rgb(0xaeb4bf))
                    .children(outline),
            );
        let workspace = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .child(tabs)
            .child(div().h(px(740.)).flex().child(editor_panel).child(outline_panel));
        let body = div()
            .h(px(776.))
            .flex()
            .child(activity)
            .child(explorer)
            .child(workspace);
        let status = div()
            .h(px(24.))
            .flex()
            .items_center()
            .justify_between()
            .px(px(10.))
            .bg(rgb(0x255a91))
            .text_color(rgb(0xffffff))
            .text_size(px(12.))
            .child("⑂ benchmark/common    ✓ 0    ⚠ 0")
            .child("Ln 1, Col 1    Spaces: 2    UTF-8    TypeScript");

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x17191f))
            .text_color(rgb(0xd6d9df))
            .font_family("Noto Sans")
            .text_size(px(13.))
            .child(body)
            .child(status)
    }
}

fn section_title(text: &'static str) -> impl IntoElement {
    div()
        .h(px(36.))
        .flex()
        .items_center()
        .px(px(12.))
        .border_b_1()
        .border_color(rgb(0x282c35))
        .text_size(px(11.))
        .text_color(rgb(0xaab0bd))
        .child(text)
}
fn tab(name: &'static str, active: bool) -> impl IntoElement {
    div()
        .w(px(190.))
        .h_full()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(14.))
        .border_r_1()
        .border_color(rgb(0x30343e))
        .bg(if active { rgb(0x17191f) } else { rgb(0x1b1e24) })
        .child(div().text_color(rgb(0x4aa5f0)).child("TS"))
        .child(name)
        .child(if active { "×" } else { "" })
}

fn highlighted(line: &str) -> StyledText {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let regex = TOKEN.get_or_init(|| Regex::new(r#"//.*$|"[^"\n]*"|\b(?:export|interface|const|function|return|type|void|null|boolean|string|number)\b|\b\d+\b|\b(?:Item|Result|Record)\d+\b|\bformat\d+\b"#).unwrap());
    let highlights: Vec<(Range<usize>, HighlightStyle)> = regex
        .find_iter(line)
        .map(|m| {
            let token = m.as_str();
            let color = if token.starts_with("//") {
                rgb(0x636b7a)
            } else if token.starts_with('"') {
                rgb(0xc3e88d)
            } else if token.chars().all(|c| c.is_ascii_digit()) {
                rgb(0xf78c6c)
            } else if token.starts_with("Item") || token.starts_with("Result") || token.starts_with("Record")
            {
                rgb(0xffcb6b)
            } else if token.starts_with("format") {
                rgb(0x82aaff)
            } else {
                rgb(0xc792ea)
            };
            (m.range(), color.into())
        })
        .collect();
    StyledText::new(SharedString::from(line.to_owned())).with_highlights(highlights)
}

fn main() {
    let logger = Logger {
        started: Instant::now(),
        phase: std::env::var("BENCH_PHASE").unwrap_or_else(|_| "visual".into()),
        run_id: std::env::var("BENCH_RUN_ID").unwrap_or_else(|_| "manual".into()),
        path: std::env::var_os("BENCH_LOG").map(PathBuf::from),
    };
    logger.emit(json!({ "event": "process_start" }));
    let fixture = std::env::var_os("BENCH_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../.fixture"));
    let lsp = std::env::var_os("BENCH_LSP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("typescript-language-server"));
    let autorun = std::env::var("BENCH_AUTORUN").as_deref() == Ok("1");
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(1280.), px(800.)),
        };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Fullscreen(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Stackhour UI Benchmark".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                is_resizable: false,
                ..Default::default()
            },
            {
                let logger = logger.clone();
                move |window, cx| cx.new(|cx| BenchView::new(window, cx, logger, fixture, lsp, autorun))
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
