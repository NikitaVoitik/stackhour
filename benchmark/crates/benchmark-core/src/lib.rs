use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub fn scan_project(root: &Path) -> std::io::Result<Vec<String>> {
    fn visit(root: &Path, folder: &Path, files: &mut Vec<String>) -> std::io::Result<()> {
        let mut entries = fs::read_dir(folder)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files)?;
            } else if path.extension().and_then(|value| value.to_str()) == Some("ts") {
                files.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: u64,
}

pub struct LanguageServer {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
    root: PathBuf,
}

impl LanguageServer {
    pub fn start(command: &Path, root: &Path) -> Result<Self, String> {
        let mut child = Command::new(command)
            .arg("--stdio")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let input = BufWriter::new(child.stdin.take().ok_or("missing language server stdin")?);
        let output = BufReader::new(child.stdout.take().ok_or("missing language server stdout")?);
        let mut server = Self { child, input, output, next_id: 1, root: root.to_path_buf() };
        let uri = file_uri(root);
        server.request("initialize", json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": { "textDocument": { "documentSymbol": { "hierarchicalDocumentSymbolSupport": true } } },
            "workspaceFolders": [{ "uri": file_uri(root), "name": "controlled-fixture" }]
        }))?;
        server.notify("initialized", json!({}))?;
        Ok(server)
    }

    pub fn document_symbols(&mut self, relative: &str) -> Result<Vec<Symbol>, String> {
        let path = self.root.join(relative);
        let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let uri = file_uri(&path);
        self.notify("textDocument/didOpen", json!({ "textDocument": { "uri": uri, "languageId": "typescript", "version": 1, "text": text } }))?;
        let response = self.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": file_uri(&path) } }))?;
        Ok(response.get("result").and_then(Value::as_array).into_iter().flatten().map(|item| Symbol {
            name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
            kind: item.get("kind").and_then(Value::as_u64).unwrap_or_default(),
        }).collect())
    }

    pub fn process_ids(&self) -> Vec<u32> { descendants(self.child.id()) }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id; self.next_id += 1;
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let message = self.read()?;
            if message.get("id").and_then(Value::as_u64) == Some(id) { return Ok(message); }
        }
    }

    fn write(&mut self, value: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        write!(self.input, "Content-Length: {}\r\n\r\n", body.len()).map_err(|error| error.to_string())?;
        self.input.write_all(&body).map_err(|error| error.to_string())?;
        self.input.flush().map_err(|error| error.to_string())
    }

    fn read(&mut self) -> Result<Value, String> {
        let mut length = None;
        loop {
            let mut header = String::new();
            self.output.read_line(&mut header).map_err(|error| error.to_string())?;
            if header == "\r\n" { break; }
            if let Some(value) = header.strip_prefix("Content-Length:") { length = value.trim().parse::<usize>().ok(); }
        }
        let mut body = vec![0; length.ok_or("language server response missing content length")?];
        self.output.read_exact(&mut body).map_err(|error| error.to_string())?;
        serde_json::from_slice(&body).map_err(|error| error.to_string())
    }
}

impl Drop for LanguageServer { fn drop(&mut self) { let _ = self.child.kill(); } }

fn file_uri(path: &Path) -> String { format!("file://{}", path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).to_string_lossy()) }

pub fn descendants(root: u32) -> Vec<u32> {
    let mut pids = vec![root]; let mut changed = true;
    while changed {
        changed = false;
        let Ok(entries) = fs::read_dir("/proc") else { break };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            if pids.contains(&pid) { continue; }
            let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
            let parent = status.lines().find_map(|line| line.strip_prefix("PPid:").and_then(|value| value.trim().parse::<u32>().ok()));
            if parent.is_some_and(|value| pids.contains(&value)) { pids.push(pid); changed = true; }
        }
    }
    pids
}

pub fn process_memory(pids: &[u32]) -> Vec<Value> {
    pids.iter().filter_map(|pid| {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let cmd = fs::read(format!("/proc/{pid}/cmdline")).ok().map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " ")).unwrap_or_default();
        let rss_kb = status.lines().find_map(|line| line.strip_prefix("VmRSS:").and_then(|value| value.split_whitespace().next()).and_then(|value| value.parse::<u64>().ok())).unwrap_or(0);
        let group = if cmd.contains("typescript-language-server") { "typescript-language-server" } else if cmd.contains("tsserver") { "tsserver" } else { "ui-runtime" };
        Some(json!({ "pid": pid, "rssKb": rss_kb, "group": group }))
    }).collect()
}
