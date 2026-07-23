import "./style.css";

declare global {
  interface Window { bench: { scan(): Promise<string[]>; read(path: string): Promise<string>; symbols(path: string): Promise<Array<{name:string;kind:number}>>; telemetry(event: Record<string, unknown>): void; telemetryBatch(events: Array<Record<string, unknown>>): void; config: { candidate: string; autorun: boolean } } }
}

const contract = { treeRows: 40, editorRows: 42, overscan: 4, lineHeight: 20, treeHeight: 18 };
const state = { files: [] as string[], lines: [] as string[], file: "src/selected.ts", treeTop: 0, editorTop: 0 };
const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `<div class="shell">
  <aside class="activity"><div class="active">⌘</div><div>⌕</div><div>⑂</div><div>△</div></aside>
  <aside class="sidebar"><div class="section-title">EXPLORER · CONTROLLED FIXTURE</div><div class="tree"><div class="tree-content"></div></div></aside>
  <main class="main"><div class="tabs"><div class="tab active"><span class="ts">TS</span><span class="tab-name">selected.ts</span><span>×</span></div><div class="tab"><span class="ts">TS</span>alternate.ts</div></div>
    <section class="editor"><div class="editor-content"></div></section>
    <aside class="outline"><div class="section-title">OUTLINE</div><div class="outline-list"><div class="outline-item">Initializing TypeScript…</div></div></aside>
    <section class="output"><div class="output-tabs"><span class="active">OUTPUT</span><span>PROBLEMS</span><span>TERMINAL</span></div><div class="log">[benchmark] deterministic project fixture<br>[scanner] waiting…<br>[typescript] language server initializing…</div></section>
  </main>
  <footer class="status"><div><span>⑂ benchmark/common</span><span>✓ 0</span><span>⚠ 0</span></div><div><span>Ln 1, Col 1</span><span>Spaces: 2</span><span>UTF-8</span><span>TypeScript</span></div></footer>
</div>`;

const tree = document.querySelector<HTMLDivElement>(".tree-content")!;
const editor = document.querySelector<HTMLDivElement>(".editor-content")!;
const outline = document.querySelector<HTMLDivElement>(".outline-list")!;
const log = document.querySelector<HTMLDivElement>(".log")!;

const escape = (value: string) => value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
function highlight(source: string) {
  if (source.trimStart().startsWith("//")) return `<span class="comment">${escape(source)}</span>`;
  return escape(source)
    .replace(/(&quot;[^&]*?&quot;|"[^"\n]*")/g, '<span class="str">$1</span>')
    .replace(/\b(export|interface|const|function|return|type|void|null|boolean|string|number)\b/g, '<span class="kw">$1</span>')
    .replace(/\b(\d+)\b/g, '<span class="num">$1</span>')
    .replace(/\b(Item\d+|Result\d+|Record\d+)\b/g, '<span class="type">$1</span>')
    .replace(/\b(format\d+)\b/g, '<span class="fn">$1</span>');
}
function renderTree() {
  const first = Math.max(0, Math.floor(state.treeTop) - contract.overscan);
  const last = Math.min(state.files.length, Math.floor(state.treeTop) + contract.treeRows + contract.overscan);
  tree.innerHTML = state.files.slice(first, last).map((path, offset) => {
    const row = first + offset; const slash = path.lastIndexOf("/");
    return `<div class="tree-row${path === state.file ? " active" : ""}" data-path="${path}" style="transform:translateY(${(row - state.treeTop) * contract.treeHeight}px)"><span class="folder">◇</span>${escape(slash >= 0 ? path.slice(slash + 1) : path)}</div>`;
  }).join("");
}
function renderEditor() {
  const first = Math.max(0, Math.floor(state.editorTop) - contract.overscan);
  const last = Math.min(state.lines.length, Math.floor(state.editorTop) + contract.editorRows + contract.overscan);
  editor.innerHTML = state.lines.slice(first, last).map((line, offset) => {
    const row = first + offset;
    return `<div class="code-row" style="transform:translateY(${(row - state.editorTop) * contract.lineHeight}px)"><span class="line-no">${row + 1}</span><span class="code">${highlight(line)}</span></div>`;
  }).join("");
}
const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
async function openFile(path: string) {
  state.file = path; window.bench.telemetry({ event: "file_click", path });
  const text = await window.bench.read(path); state.lines = text.split("\n"); state.editorTop = 0;
  document.querySelector(".tab-name")!.textContent = path.split("/").at(-1)!; renderTree(); renderEditor();
  await frame(); window.bench.telemetry({ event: "text_presented", path });
  await frame(); window.bench.telemetry({ event: "stable_frame", path });
}
async function scrollWorkload() {
  const events: Array<Record<string, unknown>> = []; let previous = performance.now(); let worstStall = 0;
  for (let leg = 0; leg < 4; leg += 1) for (let step = 0; step < 160; step += 1) {
    await frame(); const now = performance.now(); const durationMs = now - previous; previous = now;
    events.push({ event: "frame", durationMs, workload: "scroll" });
    worstStall = Math.max(worstStall, durationMs - 16.7);
    const t = step / 159; const eased = (1 - Math.cos(Math.PI * t)) / 2; const forward = leg % 2 === 0;
    state.editorTop = (forward ? eased : 1 - eased) * 19958; state.treeTop = (forward ? eased : 1 - eased) * 5080;
    renderEditor(); renderTree();
  }
  events.push({ event: "main_thread_stall", durationMs: Math.max(0, worstStall), workload: "scroll" });
  window.bench.telemetryBatch(events);
}
async function start() {
  await frame(); window.bench.telemetry({ event: "first_frame" });
  window.bench.telemetry({ event: "project_open_requested" });
  state.files = await window.bench.scan(); renderTree();
  log.innerHTML = `[benchmark] deterministic project fixture<br>[scanner] ${state.files.length.toLocaleString()} TypeScript files<br>[typescript] language server initializing…`;
  await frame(); window.bench.telemetry({ event: "project_tree_visible" }); window.bench.telemetry({ event: "tree_presented" });
  await openFile("src/selected.ts");
  window.bench.telemetry({ event: "lsp_request", method: "textDocument/documentSymbol" });
  const symbols = await window.bench.symbols("src/selected.ts");
  window.bench.telemetry({ event: "lsp_response", count: symbols.length });
  outline.innerHTML = symbols.slice(0, 24).map((symbol) => `<div class="outline-item"><span class="symbol">◇</span>${escape(symbol.name)}</div>`).join("") || '<div class="outline-item">No symbols</div>';
  await frame(); window.bench.telemetry({ event: "outline_presented" });
  if (window.bench.config.autorun) {
    await scrollWorkload();
    for (let index = 0; index < 30; index += 1) await openFile(index % 2 ? "src/selected.ts" : "src/alternate.ts");
    window.bench.telemetry({ event: "benchmark_complete" });
  }
}
void start();
