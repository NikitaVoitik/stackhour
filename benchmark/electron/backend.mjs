import { readdir, readFile } from "node:fs/promises";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { spawn } from "node:child_process";
import { pathToFileURL } from "node:url";

let languageProcess = null;

export async function scan(root) {
  const files = [];
  async function visit(folder) {
    const entries = await readdir(folder, { withFileTypes: true });
    entries.sort((a, b) => a.name.localeCompare(b.name));
    for (const entry of entries) {
      const path = join(folder, entry.name);
      if (entry.isDirectory()) await visit(path);
      else if (entry.name.endsWith(".ts")) files.push(relative(root, path).split(sep).join("/"));
    }
  }
  await visit(root); return files.sort();
}
export const readSource = (root, path) => readFile(join(root, path), "utf8");

export async function documentSymbols(root, path, command) {
  const child = spawn(command, ["--stdio"], { cwd: root, stdio: ["pipe", "pipe", "ignore"] }); languageProcess = child;
  let buffer = Buffer.alloc(0); let sequence = 1; const waiting = new Map();
  const send = (message) => {
    const body = Buffer.from(JSON.stringify(message));
    child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`); child.stdin.write(body);
  };
  child.stdout.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (true) {
      const split = buffer.indexOf("\r\n\r\n"); if (split < 0) break;
      const length = Number(buffer.subarray(0, split).toString().match(/Content-Length: (\d+)/i)?.[1]);
      if (buffer.length < split + 4 + length) break;
      const message = JSON.parse(buffer.subarray(split + 4, split + 4 + length).toString()); buffer = buffer.subarray(split + 4 + length);
      if (message.id && waiting.has(message.id)) { waiting.get(message.id)(message); waiting.delete(message.id); }
    }
  });
  const request = (method, params) => new Promise((resolve) => { const id = sequence++; waiting.set(id, resolve); send({ jsonrpc: "2.0", id, method, params }); });
  const rootUri = pathToFileURL(root).href; const uri = pathToFileURL(join(root, path)).href;
  await request("initialize", { processId: process.pid, rootUri, capabilities: { textDocument: { documentSymbol: { hierarchicalDocumentSymbolSupport: true } } }, workspaceFolders: [{ uri: rootUri, name: "controlled-fixture" }] });
  send({ jsonrpc: "2.0", method: "initialized", params: {} });
  const text = await readSource(root, path); send({ jsonrpc: "2.0", method: "textDocument/didOpen", params: { textDocument: { uri, languageId: "typescript", version: 1, text } } });
  const response = await request("textDocument/documentSymbol", { textDocument: { uri } });
  return (response.result || []).map((symbol) => ({ name: symbol.name, kind: symbol.kind }));
}

export function languagePids() {
  if (!languageProcess?.pid) return [];
  const pids = [languageProcess.pid];
  let changed = true;
  while (changed) {
    changed = false;
    for (const name of readdirSync("/proc")) {
      if (!/^\d+$/.test(name) || pids.includes(Number(name))) continue;
      try {
        const parent = Number(readFileSync(`/proc/${name}/status`, "utf8").match(/^PPid:\s+(\d+)/m)?.[1]);
        if (pids.includes(parent)) { pids.push(Number(name)); changed = true; }
      } catch {}
    }
  }
  return pids;
}
export function stopLanguageServer() { languageProcess?.kill(); }
