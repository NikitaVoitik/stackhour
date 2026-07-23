use benchmark_core::{LanguageServer, Symbol, descendants, process_memory, scan_project};
use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Stroke, TextFormat,
    ViewportBuilder, ViewportCommand, pos2, vec2,
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const BG: Color32 = Color32::from_rgb(0x17, 0x19, 0x1f);
const MONO: FontFamily = FontFamily::Monospace;

#[derive(Clone)]
struct Logger {
    started: Instant,
    phase: String,
    run_id: String,
    path: Option<PathBuf>,
}

impl Logger {
    fn row(&self, event: Value) -> String {
        let mut row = Map::new();
        row.insert("schemaVersion".into(), json!(1));
        row.insert("candidate".into(), json!("egui"));
        row.insert("phase".into(), json!(self.phase));
        row.insert("runId".into(), json!(self.run_id));
        row.insert(
            "timestampNs".into(),
            json!(self.started.elapsed().as_nanos() as u64),
        );
        if let Some(values) = event.as_object() {
            row.extend(values.clone());
        }
        serde_json::to_string(&row).unwrap()
    }

    fn emit(&self, event: Value) {
        let line = self.row(event);
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

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Initial,
    WaitingFirst,
    WaitingScan,
    TreeNeedsPaint,
    TreePainted,
    WaitingRead,
    TextNeedsPaint,
    TextPainted,
    StableNeedsPaint,
    StablePainted,
    WaitingSymbols,
    OutlineNeedsPaint,
    OutlinePainted,
    Scrolling,
    Complete,
}

struct BenchApp {
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

impl BenchApp {
    fn new(
        context: &egui::Context,
        logger: Logger,
        fixture: PathBuf,
        lsp_command: PathBuf,
        autorun: bool,
    ) -> Self {
        install_fonts(context);
        context.set_pixels_per_point(1.0);
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
            close_sent: false,
        }
    }

    fn open_file(&mut self, path: &str, is_switch: bool) {
        self.selected = path.to_owned();
        self.current_read_is_switch = is_switch;
        self.logger.emit(json!({ "event": "file_click", "path": path }));
        let _ = self.commands.send(BackendCommand::Read(path.to_owned()));
        self.stage = Stage::WaitingRead;
    }

    fn receive(&mut self) {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                BackendMessage::Scan(files) => {
                    self.files = files;
                    self.stage = Stage::TreeNeedsPaint;
                }
                BackendMessage::Read(path, text) => {
                    self.selected = path;
                    self.lines = text.lines().map(str::to_owned).collect();
                    self.editor_top = 0.0;
                    self.stage = Stage::TextNeedsPaint;
                }
                BackendMessage::Symbols(symbols) => {
                    self.logger
                        .emit(json!({ "event": "lsp_response", "count": symbols.len() }));
                    self.symbols = symbols;
                    self.stage = Stage::OutlineNeedsPaint;
                }
                BackendMessage::Error(error) => {
                    self.logger
                        .emit(json!({ "event": "benchmark_error", "message": error }));
                    self.stage = Stage::Complete;
                }
            }
        }
    }

    fn presented_transitions(&mut self) {
        match self.stage {
            Stage::WaitingFirst => {
                self.logger.emit(json!({ "event": "first_frame" }));
                self.logger.memory("idle");
                self.logger.emit(json!({ "event": "project_open_requested" }));
                let _ = self.commands.send(BackendCommand::Scan);
                self.stage = Stage::WaitingScan;
            }
            Stage::TreePainted => {
                self.logger.emit(json!({ "event": "project_tree_visible" }));
                self.logger.emit(json!({ "event": "tree_presented" }));
                self.open_file("src/selected.ts", false);
            }
            Stage::TextPainted => {
                self.logger
                    .emit(json!({ "event": "text_presented", "path": self.selected }));
                self.stage = Stage::StableNeedsPaint;
            }
            Stage::StablePainted => {
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
            Stage::OutlinePainted => {
                self.logger.emit(json!({ "event": "outline_presented" }));
                self.logger.memory("loaded");
                if self.autorun {
                    self.stage = Stage::Scrolling;
                    self.scroll_step = 0;
                    self.last_frame = Instant::now();
                } else {
                    self.stage = Stage::Complete;
                }
            }
            _ => {}
        }
    }

    fn advance_scroll(&mut self) {
        if self.stage != Stage::Scrolling {
            return;
        }
        if self.scroll_step > 0 {
            let now = Instant::now();
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

    fn after_paint(&mut self, context: &egui::Context) {
        self.stage = match self.stage {
            Stage::Initial => Stage::WaitingFirst,
            Stage::TreeNeedsPaint => Stage::TreePainted,
            Stage::TextNeedsPaint => Stage::TextPainted,
            Stage::StableNeedsPaint => Stage::StablePainted,
            Stage::OutlineNeedsPaint => Stage::OutlinePainted,
            other => other,
        };
        match self.stage {
            Stage::WaitingScan | Stage::WaitingRead | Stage::WaitingSymbols => {
                context.request_repaint_after(Duration::from_millis(1));
            }
            Stage::WaitingFirst
            | Stage::TreePainted
            | Stage::TextPainted
            | Stage::StablePainted
            | Stage::OutlinePainted
            | Stage::Scrolling => context.request_repaint(),
            Stage::Complete if self.autorun && !self.close_sent => {
                context.send_viewport_cmd(ViewportCommand::Close);
                self.close_sent = true;
            }
            _ => {}
        }
    }

    fn draw(&self, ui: &mut egui::Ui) {
        let painter = ui.painter();
        let rect = ui.max_rect();
        painter.rect_filled(rect, 0.0, BG);
        fill(painter, 0.0, 0.0, 48.0, 776.0, 0x1d2027);
        fill(painter, 48.0, 0.0, 260.0, 776.0, 0x1c1f26);
        fill(painter, 308.0, 0.0, 972.0, 36.0, 0x1b1e24);
        fill(painter, 1060.0, 36.0, 220.0, 740.0, 0x1b1e24);
        fill(painter, 308.0, 636.0, 752.0, 140.0, 0x181a20);
        fill(painter, 0.0, 776.0, 1280.0, 24.0, 0x255a91);
        line(painter, 48.0, 0.0, 48.0, 776.0, 0x30343e);
        line(painter, 308.0, 0.0, 308.0, 776.0, 0x30343e);
        line(painter, 1060.0, 36.0, 1060.0, 776.0, 0x30343e);
        line(painter, 308.0, 36.0, 1280.0, 36.0, 0x30343e);
        line(painter, 308.0, 636.0, 1060.0, 636.0, 0x30343e);
        line(painter, 308.0, 667.0, 1060.0, 667.0, 0x282c35);

        fill(painter, 8.0, 10.0, 32.0, 32.0, 0x292d36);
        label(
            painter,
            24.0,
            26.0,
            Align2::CENTER_CENTER,
            "⌘",
            17.0,
            FontFamily::Proportional,
            0xf3f5f8,
        );
        label(
            painter,
            24.0,
            66.0,
            Align2::CENTER_CENTER,
            "⌕",
            17.0,
            FontFamily::Proportional,
            0x8c93a3,
        );
        label(
            painter,
            24.0,
            106.0,
            Align2::CENTER_CENTER,
            "⑂",
            17.0,
            FontFamily::Proportional,
            0x8c93a3,
        );
        label(
            painter,
            24.0,
            146.0,
            Align2::CENTER_CENTER,
            "△",
            17.0,
            FontFamily::Proportional,
            0x8c93a3,
        );
        label(
            painter,
            60.0,
            18.0,
            Align2::LEFT_CENTER,
            "EXPLORER · CONTROLLED FIXTURE",
            11.0,
            FontFamily::Proportional,
            0xaab0bd,
        );
        label(
            painter,
            1072.0,
            54.0,
            Align2::LEFT_CENTER,
            "OUTLINE",
            11.0,
            FontFamily::Proportional,
            0xaab0bd,
        );

        fill(painter, 308.0, 0.0, 190.0, 36.0, 0x17191f);
        label(
            painter,
            322.0,
            18.0,
            Align2::LEFT_CENTER,
            "TS",
            13.0,
            FontFamily::Proportional,
            0x4aa5f0,
        );
        label(
            painter,
            350.0,
            18.0,
            Align2::LEFT_CENTER,
            self.selected.rsplit('/').next().unwrap_or("selected.ts"),
            13.0,
            FontFamily::Proportional,
            0xf3f5f8,
        );
        label(
            painter,
            484.0,
            18.0,
            Align2::CENTER_CENTER,
            "×",
            13.0,
            FontFamily::Proportional,
            0xaeb4bf,
        );
        label(
            painter,
            512.0,
            18.0,
            Align2::LEFT_CENTER,
            "TS",
            13.0,
            FontFamily::Proportional,
            0x4aa5f0,
        );
        label(
            painter,
            540.0,
            18.0,
            Align2::LEFT_CENTER,
            "alternate.ts",
            13.0,
            FontFamily::Proportional,
            0xaeb4bf,
        );

        let tree_clip = Rect::from_min_max(pos2(48.0, 36.0), pos2(308.0, 756.0));
        let tree_painter = painter.with_clip_rect(tree_clip);
        let tree_first = (self.tree_top.floor() as usize).saturating_sub(4);
        let tree_last = (self.tree_top.floor() as usize + 44).min(self.files.len());
        for (offset, path) in self.files[tree_first..tree_last].iter().enumerate() {
            let row = tree_first + offset;
            let y = 36.0 + (row as f32 - self.tree_top) * 18.0;
            if path == &self.selected {
                fill(&tree_painter, 48.0, y, 260.0, 18.0, 0x2c3240);
            }
            label(
                &tree_painter,
                60.0,
                y + 9.0,
                Align2::LEFT_CENTER,
                "◇",
                13.0,
                MONO,
                0xe2bd75,
            );
            label(
                &tree_painter,
                78.0,
                y + 9.0,
                Align2::LEFT_CENTER,
                path.rsplit('/').next().unwrap_or(path),
                13.0,
                MONO,
                0xb7bdc9,
            );
        }

        let editor_clip = Rect::from_min_max(pos2(308.0, 36.0), pos2(1060.0, 636.0));
        let editor_painter = painter.with_clip_rect(editor_clip);
        let editor_first = (self.editor_top.floor() as usize).saturating_sub(4);
        let editor_last = (self.editor_top.floor() as usize + 34).min(self.lines.len());
        for (offset, source) in self.lines[editor_first..editor_last].iter().enumerate() {
            let row = editor_first + offset;
            let y = 36.0 + (row as f32 - self.editor_top) * 20.0;
            label(
                &editor_painter,
                352.0,
                y + 10.0,
                Align2::RIGHT_CENTER,
                &(row + 1).to_string(),
                13.0,
                MONO,
                0x565d6c,
            );
            let galley = ui.ctx().fonts_mut(|fonts| fonts.layout_job(highlighted(source)));
            editor_painter.galley(pos2(366.0, y + 1.0), galley, Color32::from_rgb(0xc7, 0xcb, 0xd4));
        }

        if self.symbols.is_empty() {
            label(
                painter,
                1068.0,
                75.0,
                Align2::LEFT_CENTER,
                "Initializing TypeScript…",
                12.0,
                MONO,
                0xaeb4bf,
            );
        } else {
            for (index, symbol) in self.symbols.iter().take(24).enumerate() {
                let y = 75.0 + index as f32 * 22.0;
                label(painter, 1068.0, y, Align2::LEFT_CENTER, "◇", 12.0, MONO, 0xc792ea);
                label(
                    painter,
                    1087.0,
                    y,
                    Align2::LEFT_CENTER,
                    &symbol.name,
                    12.0,
                    MONO,
                    0xaeb4bf,
                );
            }
        }

        label(
            painter,
            320.0,
            651.5,
            Align2::LEFT_CENTER,
            "OUTPUT",
            11.0,
            FontFamily::Proportional,
            0xffffff,
        );
        label(
            painter,
            380.0,
            651.5,
            Align2::LEFT_CENTER,
            "PROBLEMS",
            11.0,
            FontFamily::Proportional,
            0xd6d9df,
        );
        label(
            painter,
            455.0,
            651.5,
            Align2::LEFT_CENTER,
            "TERMINAL",
            11.0,
            FontFamily::Proportional,
            0xd6d9df,
        );
        label(
            painter,
            320.0,
            685.0,
            Align2::LEFT_CENTER,
            "[benchmark] deterministic project fixture",
            12.0,
            MONO,
            0x8f97a6,
        );
        label(
            painter,
            320.0,
            705.0,
            Align2::LEFT_CENTER,
            &format!("[scanner] {} TypeScript files", self.files.len()),
            12.0,
            MONO,
            0x8f97a6,
        );
        label(
            painter,
            320.0,
            725.0,
            Align2::LEFT_CENTER,
            &format!("[typescript] {} document symbols", self.symbols.len()),
            12.0,
            MONO,
            0x8f97a6,
        );
        label(
            painter,
            10.0,
            788.0,
            Align2::LEFT_CENTER,
            "⑂ benchmark/common    ✓ 0    ⚠ 0",
            12.0,
            FontFamily::Proportional,
            0xffffff,
        );
        label(
            painter,
            1270.0,
            788.0,
            Align2::RIGHT_CENTER,
            "Ln 1, Col 1    Spaces: 2    UTF-8    TypeScript",
            12.0,
            FontFamily::Proportional,
            0xffffff,
        );
    }
}

impl eframe::App for BenchApp {
    fn logic(&mut self, _context: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive();
        self.presented_transitions();
        self.advance_scroll();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.set_min_size(vec2(1280.0, 800.0));
        self.draw(ui);
        let context = ui.ctx().clone();
        self.after_paint(&context);
    }
}

fn install_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    add_font(
        &mut fonts,
        "noto-sans",
        &[
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        ],
        FontFamily::Proportional,
    );
    add_font(
        &mut fonts,
        "noto-mono",
        &[
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
            "/System/Library/Fonts/SFNSMono.ttf",
        ],
        FontFamily::Monospace,
    );
    context.set_fonts(fonts);
}

fn add_font(fonts: &mut FontDefinitions, name: &str, paths: &[&str], family: FontFamily) {
    if let Some(bytes) = paths.iter().find_map(|path| fs::read(path).ok()) {
        fonts
            .font_data
            .insert(name.into(), FontData::from_owned(bytes).into());
        fonts.families.entry(family).or_default().insert(0, name.into());
    }
}

fn fill(painter: &egui::Painter, x: f32, y: f32, width: f32, height: f32, color: u32) {
    painter.rect_filled(
        Rect::from_min_size(pos2(x, y), vec2(width, height)),
        0.0,
        rgb(color),
    );
}

fn line(painter: &egui::Painter, x1: f32, y1: f32, x2: f32, y2: f32, color: u32) {
    painter.line_segment([pos2(x1, y1), pos2(x2, y2)], Stroke::new(1.0, rgb(color)));
}

fn label(
    painter: &egui::Painter,
    x: f32,
    y: f32,
    align: Align2,
    text: &str,
    size: f32,
    family: FontFamily,
    color: u32,
) {
    painter.text(
        Pos2::new(x, y),
        align,
        text,
        FontId::new(size, family),
        rgb(color),
    );
}

fn rgb(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn highlighted(line: &str) -> egui::text::LayoutJob {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let regex = TOKEN.get_or_init(|| {
        Regex::new(r#"//.*$|"[^"\n]*"|\b(?:export|interface|const|function|return|type|void|null|boolean|string|number)\b|\b\d+\b|\b(?:Item|Result|Record)\d+\b|\bformat\d+\b"#).unwrap()
    });
    let mut job = egui::text::LayoutJob::default();
    let mut cursor = 0;
    for token in regex.find_iter(line) {
        if token.start() > cursor {
            job.append(&line[cursor..token.start()], 0.0, format(0xc7cbd4, false));
        }
        let value = token.as_str();
        let (color, italics) = if value.starts_with("//") {
            (0x636b7a, true)
        } else if value.starts_with('"') {
            (0xc3e88d, false)
        } else if value.chars().all(|ch| ch.is_ascii_digit()) {
            (0xf78c6c, false)
        } else if value.starts_with("Item") || value.starts_with("Result") || value.starts_with("Record") {
            (0xffcb6b, false)
        } else if value.starts_with("format") {
            (0x82aaff, false)
        } else {
            (0xc792ea, false)
        };
        job.append(value, 0.0, format(color, italics));
        cursor = token.end();
    }
    if cursor < line.len() {
        job.append(&line[cursor..], 0.0, format(0xc7cbd4, false));
    }
    job
}

fn format(color: u32, italics: bool) -> TextFormat {
    TextFormat {
        font_id: FontId::new(13.0, MONO),
        color: rgb(color),
        italics,
        ..Default::default()
    }
}

fn main() -> eframe::Result {
    let logger = Logger {
        started: Instant::now(),
        phase: std::env::var("BENCH_PHASE").unwrap_or_else(|_| "visual".into()),
        run_id: std::env::var("BENCH_RUN_ID").unwrap_or_else(|_| "manual".into()),
        path: std::env::var_os("BENCH_LOG").map(PathBuf::from),
    };
    logger.emit(json!({ "event": "process_start" }));
    let fixture = env_path("BENCH_FIXTURE", "../.fixture");
    let lsp = env_path("BENCH_LSP", "typescript-language-server");
    let autorun = std::env::var("BENCH_AUTORUN").as_deref() == Ok("1");
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1280.0, 800.0])
            .with_max_inner_size([1280.0, 800.0])
            .with_resizable(false)
            .with_decorations(false),
        ..Default::default()
    };
    eframe::run_native(
        "Stackhour UI Benchmark",
        options,
        Box::new(move |creation| {
            Ok(Box::new(BenchApp::new(
                &creation.egui_ctx,
                logger,
                fixture,
                lsp,
                autorun,
            )))
        }),
    )
}

fn env_path(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(fallback).into())
}
