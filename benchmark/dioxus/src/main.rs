use benchmark_core::{LanguageServer, Symbol, descendants, process_memory, scan_project};
use dioxus::desktop::{Config, LogicalSize, WindowBuilder, window};
use dioxus::prelude::*;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const STYLE: &str = include_str!("style.css");
static LOGGER: OnceLock<Logger> = OnceLock::new();

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
        row.insert("candidate".into(), json!("dioxus-desktop"));
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

    fn memory(&self, label: &str) {
        self.emit(json!({
            "event": "memory_sample",
            "label": label,
            "processes": process_memory(&descendants(std::process::id()))
        }));
    }
}

fn main() {
    let logger = Logger {
        started: Instant::now(),
        phase: std::env::var("BENCH_PHASE").unwrap_or_else(|_| "visual".into()),
        run_id: std::env::var("BENCH_RUN_ID").unwrap_or_else(|_| "manual".into()),
        path: std::env::var_os("BENCH_LOG").map(PathBuf::from),
    };
    logger.emit(json!({ "event": "process_start" }));
    let _ = LOGGER.set(logger);
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::default().with_window(
                WindowBuilder::new()
                    .with_title("Stackhour UI Benchmark")
                    .with_inner_size(LogicalSize::new(1280.0, 800.0))
                    .with_min_inner_size(LogicalSize::new(1280.0, 800.0))
                    .with_max_inner_size(LogicalSize::new(1280.0, 800.0))
                    .with_resizable(false)
                    .with_decorations(false),
            ),
        )
        .launch(App);
}

#[component]
fn App() -> Element {
    let files = use_signal(Vec::<String>::new);
    let lines = use_signal(Vec::<String>::new);
    let symbols = use_signal(Vec::<Symbol>::new);
    let selected = use_signal(|| "src/selected.ts".to_owned());
    let tree_top = use_signal(|| 0.0_f64);
    let editor_top = use_signal(|| 0.0_f64);
    let scanner_text = use_signal(|| "waiting…".to_owned());
    let language_text = use_signal(|| "language server initializing…".to_owned());
    let desktop = window();

    use_future(move || {
        let desktop = desktop.clone();
        async move {
            let logger = LOGGER.get().expect("benchmark logger initialized").clone();
            let fixture = std::env::var_os("BENCH_FIXTURE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("../.fixture"));
            let lsp_command = std::env::var_os("BENCH_LSP")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("typescript-language-server"));
            let lsp = Arc::new(Mutex::new(None::<LanguageServer>));
            let autorun = std::env::var("BENCH_AUTORUN").as_deref() == Ok("1");

            let result = run_benchmark(
                &logger,
                &fixture,
                &lsp_command,
                lsp,
                autorun,
                files,
                lines,
                symbols,
                selected,
                tree_top,
                editor_top,
                scanner_text,
                language_text,
            )
            .await;
            if let Err(error) = result {
                logger.emit(json!({ "event": "benchmark_error", "message": error }));
                if autorun {
                    desktop.close();
                }
            } else if autorun {
                desktop.close();
            }
        }
    });

    let tree_value = tree_top();
    let editor_value = editor_top();
    let selected_value = selected();
    let tree_first = (tree_value.floor() as usize).saturating_sub(4);
    let tree_rows: Vec<_> = {
        let data = files.read();
        let last = (tree_value.floor() as usize + 44).min(data.len());
        data[tree_first..last]
            .iter()
            .enumerate()
            .map(|(offset, path)| {
                (
                    tree_first + offset,
                    path.clone(),
                    path.rsplit('/').next().unwrap_or(path).to_owned(),
                )
            })
            .collect()
    };
    let editor_first = (editor_value.floor() as usize).saturating_sub(4);
    let editor_rows: Vec<_> = {
        let data = lines.read();
        let last = (editor_value.floor() as usize + 34).min(data.len());
        data[editor_first..last]
            .iter()
            .enumerate()
            .map(|(offset, source)| (editor_first + offset, highlight(source)))
            .collect()
    };
    let outline: Vec<_> = symbols.read().iter().take(24).cloned().collect();
    let selected_name = selected_value.rsplit('/').next().unwrap_or("selected.ts");

    rsx! {
        style { {STYLE} }
        div { id: "app", class: "shell",
            aside { class: "activity",
                div { class: "active", "⌘" } div { "⌕" } div { "⑂" } div { "△" }
            }
            aside { class: "sidebar",
                div { class: "section-title", "EXPLORER · CONTROLLED FIXTURE" }
                div { class: "tree", div { class: "tree-content",
                    for (row, path, name) in tree_rows {
                        div {
                            key: "{path}",
                            class: if path == selected_value { "tree-row active" } else { "tree-row" },
                            "data-path": "{path}",
                            style: "transform:translateY({(row as f64 - tree_value) * 18.0}px)",
                            span { class: "folder", "◇" }
                            "{name}"
                        }
                    }
                }}
            }
            main { class: "main",
                div { class: "tabs",
                    div { class: "tab active", span { class: "ts", "TS" } span { class: "tab-name", "{selected_name}" } span { "×" } }
                    div { class: "tab", span { class: "ts", "TS" } "alternate.ts" }
                }
                section { class: "editor", div { class: "editor-content",
                    for (row, html) in editor_rows {
                        div {
                            key: "{row}",
                            class: "code-row",
                            style: "transform:translateY({(row as f64 - editor_value) * 20.0}px)",
                            span { class: "line-no", "{row + 1}" }
                            span { class: "code", dangerous_inner_html: "{html}" }
                        }
                    }
                }}
                aside { class: "outline",
                    div { class: "section-title", "OUTLINE" }
                    div { class: "outline-list",
                        if outline.is_empty() {
                            div { class: "outline-item", "Initializing TypeScript…" }
                        } else {
                            for (index, symbol) in outline.into_iter().enumerate() {
                                div { key: "{symbol.name}-{index}", class: "outline-item", span { class: "symbol", "◇" } "{symbol.name}" }
                            }
                        }
                    }
                }
                section { class: "output",
                    div { class: "output-tabs", span { class: "active", "OUTPUT" } span { "PROBLEMS" } span { "TERMINAL" } }
                    div { class: "log", "[benchmark] deterministic project fixture" br {}
                        "[scanner] {scanner_text}" br {} "[typescript] {language_text}"
                    }
                }
            }
            footer { class: "status",
                div { span { "⑂ benchmark/common" } span { "✓ 0" } span { "⚠ 0" } }
                div { span { "Ln 1, Col 1" } span { "Spaces: 2" } span { "UTF-8" } span { "TypeScript" } }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_benchmark(
    logger: &Logger,
    fixture: &PathBuf,
    lsp_command: &PathBuf,
    lsp: Arc<Mutex<Option<LanguageServer>>>,
    autorun: bool,
    mut files: Signal<Vec<String>>,
    lines: Signal<Vec<String>>,
    mut symbols: Signal<Vec<Symbol>>,
    selected: Signal<String>,
    mut tree_top: Signal<f64>,
    mut editor_top: Signal<f64>,
    mut scanner_text: Signal<String>,
    mut language_text: Signal<String>,
) -> Result<(), String> {
    frame().await;
    logger.emit(json!({ "event": "first_frame" }));
    logger.memory("idle");
    logger.emit(json!({ "event": "project_open_requested" }));
    let scan_root = fixture.clone();
    let scanned = tokio::task::spawn_blocking(move || scan_project(&scan_root))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    logger.emit(json!({ "event": "project_scan_complete", "count": scanned.len() }));
    scanner_text.set(format!("{} TypeScript files", scanned.len()));
    files.set(scanned);
    frame().await;
    logger.emit(json!({ "event": "project_tree_visible" }));
    logger.emit(json!({ "event": "tree_presented" }));
    open_file(logger, fixture, "src/selected.ts", selected, lines, editor_top).await?;
    logger.emit(json!({ "event": "lsp_request", "method": "textDocument/documentSymbol" }));
    let lsp_path = lsp_command.clone();
    let lsp_root = fixture.clone();
    let lsp_state = lsp.clone();
    let document_symbols = tokio::task::spawn_blocking(move || {
        let mut guard = lsp_state.lock().map_err(|error| error.to_string())?;
        if guard.is_none() {
            *guard = Some(LanguageServer::start(&lsp_path, &lsp_root)?);
        }
        guard.as_mut().unwrap().document_symbols("src/selected.ts")
    })
    .await
    .map_err(|error| error.to_string())??;
    logger.emit(json!({ "event": "lsp_response", "count": document_symbols.len() }));
    language_text.set(format!("{} document symbols", document_symbols.len()));
    symbols.set(document_symbols);
    frame().await;
    logger.emit(json!({ "event": "outline_presented" }));
    logger.memory("loaded");
    if autorun {
        let mut previous = Instant::now();
        let mut durations = Vec::with_capacity(640);
        for leg in 0..4 {
            for step in 0..160 {
                frame().await;
                let now = Instant::now();
                durations.push((now - previous).as_secs_f64() * 1000.0);
                previous = now;
                let t = step as f64 / 159.0;
                let eased = (1.0 - (std::f64::consts::PI * t).cos()) / 2.0;
                let value = if leg % 2 == 0 { eased } else { 1.0 - eased };
                editor_top.set(value * 19_970.0);
                tree_top.set(value * 5_080.0);
            }
        }
        for duration in &durations {
            logger.emit(json!({ "event": "frame", "durationMs": duration, "workload": "scroll" }));
        }
        let worst = durations.iter().copied().fold(0.0_f64, f64::max);
        logger.emit(json!({
            "event": "main_thread_stall",
            "durationMs": (worst - 16.7).max(0.0),
            "workload": "scroll"
        }));
        for index in 0..30 {
            let path = if index % 2 == 0 {
                "src/alternate.ts"
            } else {
                "src/selected.ts"
            };
            open_file(logger, fixture, path, selected, lines, editor_top).await?;
        }
        logger.emit(json!({ "event": "benchmark_complete" }));
        logger.memory("complete");
    }
    drop(lsp);
    Ok(())
}

async fn open_file(
    logger: &Logger,
    fixture: &PathBuf,
    path: &str,
    mut selected: Signal<String>,
    mut lines: Signal<Vec<String>>,
    mut editor_top: Signal<f64>,
) -> Result<(), String> {
    selected.set(path.to_owned());
    logger.emit(json!({ "event": "file_click", "path": path }));
    let absolute = fixture.join(path);
    let path_owned = path.to_owned();
    let source = tokio::task::spawn_blocking(move || fs::read_to_string(absolute))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    logger.emit(json!({
        "event": "disk_read_complete",
        "path": path_owned,
        "bytes": source.len()
    }));
    lines.set(source.lines().map(str::to_owned).collect());
    editor_top.set(0.0);
    frame().await;
    logger.emit(json!({ "event": "text_presented", "path": path }));
    frame().await;
    logger.emit(json!({ "event": "stable_frame", "path": path }));
    Ok(())
}

async fn frame() {
    let _ = document::eval("await new Promise(requestAnimationFrame); return performance.now();").await;
}

fn highlight(source: &str) -> String {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let regex = TOKEN.get_or_init(|| {
        Regex::new(r#"//.*$|"[^"\n]*"|\b(?:export|interface|const|function|return|type|void|null|boolean|string|number)\b|\b\d+\b|\b(?:Item|Result|Record)\d+\b|\bformat\d+\b"#).unwrap()
    });
    let mut result = String::new();
    let mut cursor = 0;
    for token in regex.find_iter(source) {
        result.push_str(&escape(&source[cursor..token.start()]));
        let value = token.as_str();
        let class = if value.starts_with("//") {
            "comment"
        } else if value.starts_with('"') {
            "str"
        } else if value.chars().all(|ch| ch.is_ascii_digit()) {
            "num"
        } else if value.starts_with("Item") || value.starts_with("Result") || value.starts_with("Record") {
            "type"
        } else if value.starts_with("format") {
            "fn"
        } else {
            "kw"
        };
        result.push_str(&format!("<span class=\"{class}\">{}</span>", escape(value)));
        cursor = token.end();
    }
    result.push_str(&escape(&source[cursor..]));
    result
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
