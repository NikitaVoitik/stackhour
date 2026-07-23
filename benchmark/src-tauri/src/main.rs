use benchmark_core::{descendants, process_memory, scan_project as scan_fixture, LanguageServer, Symbol};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;
use tauri::{AppHandle, Manager, State};

struct BenchState {
    candidate: &'static str,
    phase: String,
    run_id: String,
    started: Instant,
    fixture: PathBuf,
    lsp_command: PathBuf,
    log_path: Option<PathBuf>,
    lsp: Mutex<Option<LanguageServer>>,
}

impl BenchState {
    fn emit(&self, row: Value) {
        let mut object = Map::new();
        object.insert("schemaVersion".into(), json!(1)); object.insert("candidate".into(), json!(self.candidate));
        object.insert("phase".into(), json!(self.phase)); object.insert("runId".into(), json!(self.run_id));
        object.insert("timestampNs".into(), json!(self.started.elapsed().as_nanos() as u64));
        if let Some(values) = row.as_object() { object.extend(values.clone()); }
        let line = serde_json::to_string(&object).unwrap();
        if let Some(path) = &self.log_path { if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) { let _ = writeln!(file, "{line}"); } }
        else { println!("{line}"); }
    }

    fn memory_sample(&self, label: &str) {
        let mut pids = descendants(std::process::id());
        if let Ok(lsp) = self.lsp.lock() { if let Some(server) = lsp.as_ref() { for pid in server.process_ids() { if !pids.contains(&pid) { pids.push(pid); } } } }
        self.emit(json!({ "event": "memory_sample", "label": label, "processes": process_memory(&pids) }));
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Config { candidate: &'static str, autorun: bool }

#[tauri::command]
fn config(state: State<'_, BenchState>) -> Config { Config { candidate: state.candidate, autorun: std::env::var("BENCH_AUTORUN").as_deref() == Ok("1") } }

#[tauri::command]
fn scan_project(state: State<'_, BenchState>) -> Result<Vec<String>, String> {
    let files = scan_fixture(&state.fixture).map_err(|error| error.to_string())?;
    state.emit(json!({ "event": "project_scan_complete", "count": files.len() })); Ok(files)
}

#[tauri::command]
fn read_source(path: String, state: State<'_, BenchState>) -> Result<String, String> {
    let text = fs::read_to_string(state.fixture.join(&path)).map_err(|error| error.to_string())?;
    state.emit(json!({ "event": "disk_read_complete", "path": path, "bytes": text.len() })); Ok(text)
}

#[tauri::command]
fn document_symbols(path: String, state: State<'_, BenchState>) -> Result<Vec<Symbol>, String> {
    let mut guard = state.lsp.lock().map_err(|error| error.to_string())?;
    if guard.is_none() { *guard = Some(LanguageServer::start(&state.lsp_command, &state.fixture)?); }
    guard.as_mut().unwrap().document_symbols(&path)
}

#[tauri::command]
fn telemetry(row: Value, app: AppHandle, state: State<'_, BenchState>) {
    let event = row.get("event").and_then(Value::as_str).unwrap_or_default().to_owned(); state.emit(row);
    if event == "first_frame" { state.memory_sample("idle"); }
    if event == "outline_presented" { state.memory_sample("loaded"); }
    if event == "benchmark_complete" { state.memory_sample("complete"); app.exit(0); }
}

#[tauri::command]
fn telemetry_batch(rows: Vec<Value>, state: State<'_, BenchState>) { for row in rows { state.emit(row); } }

fn main() {
    let state = BenchState {
        candidate: "svelte-tauri", phase: std::env::var("BENCH_PHASE").unwrap_or_else(|_| "visual".into()),
        run_id: std::env::var("BENCH_RUN_ID").unwrap_or_else(|_| "manual".into()), started: Instant::now(),
        fixture: std::env::var_os("BENCH_FIXTURE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("../.fixture")),
        lsp_command: std::env::var_os("BENCH_LSP").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("typescript-language-server")),
        log_path: std::env::var_os("BENCH_LOG").map(PathBuf::from), lsp: Mutex::new(None),
    };
    state.emit(json!({ "event": "process_start" }));
    tauri::Builder::default().manage(state)
        .invoke_handler(tauri::generate_handler![config, scan_project, read_source, document_symbols, telemetry, telemetry_batch])
        .setup(|app| { app.get_webview_window("main").unwrap().set_title("Stackhour UI Benchmark")?; Ok(()) })
        .run(tauri::generate_context!()).expect("Tauri benchmark failed");
}
