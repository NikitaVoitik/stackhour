use benchmark_core::{LanguageServer, Symbol, descendants, process_memory, scan_project};
use iced::alignment::{Horizontal, Vertical};
use iced::font::{Family, Weight};
use iced::mouse;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced::{Color, Element, Fill, Font, Point, Rectangle, Renderer, Size, Subscription, Task, Theme, window};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
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
    fn emit(&self, event: Value) {
        let mut row = Map::new();
        row.insert("schemaVersion".into(), json!(1));
        row.insert("candidate".into(), json!("iced"));
        row.insert("phase".into(), json!(self.phase));
        row.insert("runId".into(), json!(self.run_id));
        row.insert(
            "timestampNs".into(),
            json!(self.started.elapsed().as_nanos() as u64),
        );
        if let Some(values) = event.as_object() {
            row.extend(values.clone());
        }
        let line = serde_json::to_string(&row).unwrap();
        if let Some(path) = &self.path {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        } else {
            println!("{line}");
        }
    }

    fn frames(&self, values: &[f64]) {
        for value in values {
            self.emit(json!({ "event": "frame", "durationMs": value, "workload": "scroll" }));
        }
    }

    fn memory(&self, label: &str) {
        self.emit(json!({
            "event": "memory_sample",
            "label": label,
            "processes": process_memory(&descendants(std::process::id()))
        }));
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Initial,
    WaitingFirst,
    WaitingScan,
    TreeNeedsFrame,
    TreePresented,
    WaitingRead,
    TextNeedsFrame,
    TextPresented,
    StableNeedsFrame,
    StablePresented,
    WaitingSymbols,
    OutlineNeedsFrame,
    OutlinePresented,
    Scrolling,
    Complete,
}

#[derive(Debug, Clone)]
enum Message {
    Frame(Instant),
}

struct Bench {
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
    close_sent: bool,
}

impl Bench {
    fn new() -> (Self, Task<Message>) {
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
        let lsp_command = std::env::var_os("BENCH_LSP")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("typescript-language-server"));
        let (command_tx, command_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let worker_logger = logger.clone();
        thread::spawn(move || {
            let mut lsp: Option<LanguageServer> = None;
            while let Ok(command) = command_rx.recv() {
                let message = match command {
                    BackendCommand::Scan => match scan_project(&fixture) {
                        Ok(files) => {
                            worker_logger.emit(json!({
                                "event": "project_scan_complete",
                                "count": files.len()
                            }));
                            BackendMessage::Scan(files)
                        }
                        Err(error) => BackendMessage::Error(error.to_string()),
                    },
                    BackendCommand::Read(relative) => match fs::read_to_string(fixture.join(&relative)) {
                        Ok(text) => {
                            worker_logger.emit(json!({
                                "event": "disk_read_complete",
                                "path": relative,
                                "bytes": text.len()
                            }));
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
                if message_tx.send(message).is_err() {
                    break;
                }
            }
        });
        (
            Self {
                logger,
                commands: command_tx,
                messages: message_rx,
                autorun: std::env::var("BENCH_AUTORUN").as_deref() == Ok("1"),
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
                close_sent: false,
            },
            Task::none(),
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.stage == Stage::Complete && !self.autorun {
            Subscription::none()
        } else {
            window::frames().map(Message::Frame)
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let Message::Frame(now) = message;
        self.receive();
        self.presented(now);
        self.after_update();
        if self.stage == Stage::Complete && self.autorun && !self.close_sent {
            self.close_sent = true;
            return window::latest().and_then(window::close);
        }
        Task::none()
    }

    fn receive(&mut self) {
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

    fn presented(&mut self, now: Instant) {
        match self.stage {
            Stage::WaitingFirst => {
                self.logger.emit(json!({ "event": "first_frame" }));
                self.logger.memory("idle");
                self.logger.emit(json!({ "event": "project_open_requested" }));
                let _ = self.commands.send(BackendCommand::Scan);
                self.stage = Stage::WaitingScan;
            }
            Stage::TreePresented => {
                self.logger.emit(json!({ "event": "project_tree_visible" }));
                self.logger.emit(json!({ "event": "tree_presented" }));
                self.open_file("src/selected.ts", false);
            }
            Stage::TextPresented => {
                self.logger
                    .emit(json!({ "event": "text_presented", "path": self.selected }));
                self.stage = Stage::StableNeedsFrame;
            }
            Stage::StablePresented => {
                self.logger
                    .emit(json!({ "event": "stable_frame", "path": self.selected }));
                if self.current_read_is_switch {
                    self.switch_index += 1;
                    if self.switch_index < 30 {
                        let path = if self.switch_index % 2 == 0 {
                            "src/alternate.ts"
                        } else {
                            "src/selected.ts"
                        };
                        self.open_file(path, true);
                    } else {
                        self.logger.emit(json!({ "event": "benchmark_complete" }));
                        self.logger.memory("complete");
                        self.stage = Stage::Complete;
                    }
                } else {
                    self.logger.emit(json!({
                        "event": "lsp_request",
                        "method": "textDocument/documentSymbol"
                    }));
                    let _ = self
                        .commands
                        .send(BackendCommand::Symbols("src/selected.ts".into()));
                    self.stage = Stage::WaitingSymbols;
                }
            }
            Stage::OutlinePresented => {
                self.logger.emit(json!({ "event": "outline_presented" }));
                self.logger.memory("loaded");
                if self.autorun {
                    self.stage = Stage::Scrolling;
                    self.scroll_step = 0;
                    self.last_frame = now;
                } else {
                    self.stage = Stage::Complete;
                }
            }
            Stage::Scrolling => self.scroll(now),
            _ => {}
        }
    }

    fn scroll(&mut self, now: Instant) {
        if self.scroll_step > 0 {
            self.frame_times
                .push((now - self.last_frame).as_secs_f64() * 1000.0);
            self.last_frame = now;
        }
        if self.scroll_step < 640 {
            let leg = self.scroll_step / 160;
            let step = self.scroll_step % 160;
            let t = step as f32 / 159.0;
            let eased = (1.0 - (std::f32::consts::PI * t).cos()) / 2.0;
            let value = if leg % 2 == 0 { eased } else { 1.0 - eased };
            self.editor_top = value * 19_970.0;
            self.tree_top = value * 5_080.0;
            self.scroll_step += 1;
        } else {
            let worst = self.frame_times.iter().copied().fold(0.0_f64, f64::max);
            self.logger.frames(&self.frame_times);
            self.logger.emit(json!({
                "event": "main_thread_stall",
                "durationMs": (worst - 16.7).max(0.0),
                "workload": "scroll"
            }));
            self.switch_index = 0;
            self.open_file("src/alternate.ts", true);
        }
    }

    fn open_file(&mut self, path: &str, is_switch: bool) {
        self.selected = path.to_owned();
        self.current_read_is_switch = is_switch;
        self.logger.emit(json!({ "event": "file_click", "path": path }));
        let _ = self.commands.send(BackendCommand::Read(path.to_owned()));
        self.stage = Stage::WaitingRead;
    }

    fn after_update(&mut self) {
        self.stage = match self.stage {
            Stage::Initial => Stage::WaitingFirst,
            Stage::TreeNeedsFrame => Stage::TreePresented,
            Stage::TextNeedsFrame => Stage::TextPresented,
            Stage::StableNeedsFrame => Stage::StablePresented,
            Stage::OutlineNeedsFrame => Stage::OutlinePresented,
            other => other,
        };
    }

    fn view(&self) -> Element<'_, Message> {
        Canvas::new(Scene { bench: self }).width(Fill).height(Fill).into()
    }
}

struct Scene<'a> {
    bench: &'a Bench,
}

impl canvas::Program<Message> for Scene<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        self.draw_shell(&mut frame);
        vec![frame.into_geometry()]
    }
}

impl Scene<'_> {
    fn draw_shell(&self, frame: &mut Frame) {
        fill(frame, 0.0, 0.0, 1280.0, 800.0, 0x17191f);
        fill(frame, 0.0, 0.0, 48.0, 776.0, 0x1d2027);
        fill(frame, 48.0, 0.0, 260.0, 776.0, 0x1c1f26);
        fill(frame, 308.0, 0.0, 972.0, 36.0, 0x1b1e24);
        fill(frame, 1060.0, 36.0, 220.0, 740.0, 0x1b1e24);
        fill(frame, 308.0, 636.0, 752.0, 140.0, 0x181a20);
        fill(frame, 0.0, 776.0, 1280.0, 24.0, 0x255a91);
        stroke(frame, 48.0, 0.0, 48.0, 776.0, 0x30343e);
        stroke(frame, 308.0, 0.0, 308.0, 776.0, 0x30343e);
        stroke(frame, 1060.0, 36.0, 1060.0, 776.0, 0x30343e);
        stroke(frame, 308.0, 36.0, 1280.0, 36.0, 0x30343e);
        stroke(frame, 308.0, 636.0, 1060.0, 636.0, 0x30343e);
        stroke(frame, 308.0, 667.0, 1060.0, 667.0, 0x282c35);
        fill(frame, 8.0, 10.0, 32.0, 32.0, 0x292d36);
        text(frame, 24.0, 26.0, "⌘", 17.0, prop(), 0xf3f5f8, Horizontal::Center);
        text(frame, 24.0, 66.0, "⌕", 17.0, prop(), 0x8c93a3, Horizontal::Center);
        text(
            frame,
            24.0,
            106.0,
            "⑂",
            17.0,
            prop(),
            0x8c93a3,
            Horizontal::Center,
        );
        text(
            frame,
            24.0,
            146.0,
            "△",
            17.0,
            prop(),
            0x8c93a3,
            Horizontal::Center,
        );
        text(
            frame,
            60.0,
            18.0,
            "EXPLORER · CONTROLLED FIXTURE",
            11.0,
            prop(),
            0xaab0bd,
            Horizontal::Left,
        );
        text(
            frame,
            1072.0,
            54.0,
            "OUTLINE",
            11.0,
            prop(),
            0xaab0bd,
            Horizontal::Left,
        );
        fill(frame, 308.0, 0.0, 190.0, 36.0, 0x17191f);
        text(frame, 322.0, 18.0, "TS", 13.0, prop(), 0x4aa5f0, Horizontal::Left);
        text(
            frame,
            350.0,
            18.0,
            self.bench.selected.rsplit('/').next().unwrap_or("selected.ts"),
            13.0,
            prop(),
            0xf3f5f8,
            Horizontal::Left,
        );
        text(
            frame,
            484.0,
            18.0,
            "×",
            13.0,
            prop(),
            0xaeb4bf,
            Horizontal::Center,
        );
        text(frame, 512.0, 18.0, "TS", 13.0, prop(), 0x4aa5f0, Horizontal::Left);
        text(
            frame,
            540.0,
            18.0,
            "alternate.ts",
            13.0,
            prop(),
            0xaeb4bf,
            Horizontal::Left,
        );

        let tree_first = (self.bench.tree_top.floor() as usize).saturating_sub(4);
        let tree_last = (self.bench.tree_top.floor() as usize + 44).min(self.bench.files.len());
        for (offset, path) in self.bench.files[tree_first..tree_last].iter().enumerate() {
            let row = tree_first + offset;
            let y = 36.0 + (row as f32 - self.bench.tree_top) * 18.0;
            if !(27.0..765.0).contains(&y) {
                continue;
            }
            if path == &self.bench.selected {
                fill(frame, 48.0, y, 260.0, 18.0, 0x2c3240);
            }
            text(
                frame,
                60.0,
                y + 9.0,
                "◇",
                13.0,
                mono(),
                0xe2bd75,
                Horizontal::Left,
            );
            text(
                frame,
                78.0,
                y + 9.0,
                path.rsplit('/').next().unwrap_or(path),
                13.0,
                mono(),
                0xb7bdc9,
                Horizontal::Left,
            );
        }

        let editor_first = (self.bench.editor_top.floor() as usize).saturating_sub(4);
        let editor_last = (self.bench.editor_top.floor() as usize + 34).min(self.bench.lines.len());
        for (offset, source) in self.bench.lines[editor_first..editor_last].iter().enumerate() {
            let row = editor_first + offset;
            let y = 36.0 + (row as f32 - self.bench.editor_top) * 20.0;
            if !(26.0..646.0).contains(&y) {
                continue;
            }
            text(
                frame,
                352.0,
                y + 10.0,
                &(row + 1).to_string(),
                13.0,
                mono(),
                0x565d6c,
                Horizontal::Right,
            );
            highlighted(frame, 366.0, y + 10.0, source);
        }

        if self.bench.symbols.is_empty() {
            text(
                frame,
                1068.0,
                75.0,
                "Initializing TypeScript…",
                12.0,
                mono(),
                0xaeb4bf,
                Horizontal::Left,
            );
        } else {
            for (index, symbol) in self.bench.symbols.iter().take(24).enumerate() {
                let y = 75.0 + index as f32 * 22.0;
                text(frame, 1068.0, y, "◇", 12.0, mono(), 0xc792ea, Horizontal::Left);
                text(
                    frame,
                    1087.0,
                    y,
                    &symbol.name,
                    12.0,
                    mono(),
                    0xaeb4bf,
                    Horizontal::Left,
                );
            }
        }
        text(
            frame,
            320.0,
            651.5,
            "OUTPUT",
            11.0,
            prop(),
            0xffffff,
            Horizontal::Left,
        );
        text(
            frame,
            380.0,
            651.5,
            "PROBLEMS",
            11.0,
            prop(),
            0xd6d9df,
            Horizontal::Left,
        );
        text(
            frame,
            455.0,
            651.5,
            "TERMINAL",
            11.0,
            prop(),
            0xd6d9df,
            Horizontal::Left,
        );
        text(
            frame,
            320.0,
            685.0,
            "[benchmark] deterministic project fixture",
            12.0,
            mono(),
            0x8f97a6,
            Horizontal::Left,
        );
        text(
            frame,
            320.0,
            705.0,
            &format!("[scanner] {} TypeScript files", self.bench.files.len()),
            12.0,
            mono(),
            0x8f97a6,
            Horizontal::Left,
        );
        text(
            frame,
            320.0,
            725.0,
            &format!("[typescript] {} document symbols", self.bench.symbols.len()),
            12.0,
            mono(),
            0x8f97a6,
            Horizontal::Left,
        );
        text(
            frame,
            10.0,
            788.0,
            "⑂ benchmark/common    ✓ 0    ⚠ 0",
            12.0,
            prop(),
            0xffffff,
            Horizontal::Left,
        );
        text(
            frame,
            1270.0,
            788.0,
            "Ln 1, Col 1    Spaces: 2    UTF-8    TypeScript",
            12.0,
            prop(),
            0xffffff,
            Horizontal::Right,
        );
    }
}

fn fill(frame: &mut Frame, x: f32, y: f32, width: f32, height: f32, color: u32) {
    frame.fill_rectangle(Point::new(x, y), Size::new(width, height), rgb(color));
}

fn stroke(frame: &mut Frame, x1: f32, y1: f32, x2: f32, y2: f32, color: u32) {
    frame.stroke(
        &Path::line(Point::new(x1, y1), Point::new(x2, y2)),
        Stroke::default().with_width(1.0).with_color(rgb(color)),
    );
}

fn text(
    frame: &mut Frame,
    x: f32,
    y: f32,
    content: &str,
    size: f32,
    font: Font,
    color: u32,
    horizontal_alignment: Horizontal,
) {
    frame.fill_text(canvas::Text {
        content: content.to_owned(),
        position: Point::new(x, y),
        color: rgb(color),
        size: size.into(),
        font,
        align_x: horizontal_alignment.into(),
        align_y: Vertical::Center,
        ..Default::default()
    });
}

fn highlighted(frame: &mut Frame, x: f32, y: f32, source: &str) {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let regex = TOKEN.get_or_init(|| {
        Regex::new(r#"//.*$|"[^"\n]*"|\b(?:export|interface|const|function|return|type|void|null|boolean|string|number)\b|\b\d+\b|\b(?:Item|Result|Record)\d+\b|\bformat\d+\b"#).unwrap()
    });
    let mut cursor = 0;
    for token in regex.find_iter(source) {
        if token.start() > cursor {
            text(
                frame,
                x + cursor as f32 * 7.8,
                y,
                &source[cursor..token.start()],
                13.0,
                mono(),
                0xc7cbd4,
                Horizontal::Left,
            );
        }
        let value = token.as_str();
        let color = if value.starts_with("//") {
            0x636b7a
        } else if value.starts_with('"') {
            0xc3e88d
        } else if value.chars().all(|ch| ch.is_ascii_digit()) {
            0xf78c6c
        } else if value.starts_with("Item") || value.starts_with("Result") || value.starts_with("Record") {
            0xffcb6b
        } else if value.starts_with("format") {
            0x82aaff
        } else {
            0xc792ea
        };
        text(
            frame,
            x + token.start() as f32 * 7.8,
            y,
            value,
            13.0,
            mono(),
            color,
            Horizontal::Left,
        );
        cursor = token.end();
    }
    if cursor < source.len() {
        text(
            frame,
            x + cursor as f32 * 7.8,
            y,
            &source[cursor..],
            13.0,
            mono(),
            0xc7cbd4,
            Horizontal::Left,
        );
    }
}

fn mono() -> Font {
    Font {
        family: Family::Monospace,
        ..Font::default()
    }
}

fn prop() -> Font {
    Font {
        family: Family::SansSerif,
        weight: Weight::Normal,
        ..Font::default()
    }
}

fn rgb(value: u32) -> Color {
    Color::from_rgb8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn main() -> iced::Result {
    iced::application(Bench::new, Bench::update, Bench::view)
        .subscription(Bench::subscription)
        .window(window::Settings {
            size: Size::new(1280.0, 800.0),
            min_size: Some(Size::new(1280.0, 800.0)),
            max_size: Some(Size::new(1280.0, 800.0)),
            resizable: false,
            decorations: false,
            ..Default::default()
        })
        .run()
}
