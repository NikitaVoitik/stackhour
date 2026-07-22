<script lang="ts">
  import { onMount, tick } from "svelte";

  declare global {
    interface Window { bench: { scan(): Promise<string[]>; read(path: string): Promise<string>; symbols(path: string): Promise<Array<{name:string;kind:number}>>; telemetry(event: Record<string, unknown>): void; telemetryBatch(events: Array<Record<string, unknown>>): void; config: { candidate: string; autorun: boolean } } }
  }

  const contract = { treeRows: 40, editorRows: 30, overscan: 4, lineHeight: 20, treeHeight: 18 };
  let files: string[] = []; let lines: string[] = []; let selectedFile = "src/selected.ts";
  let treeTop = 0; let editorTop = 0; let symbols: Array<{name:string;kind:number}> = [];
  let scannerText = "waiting…"; let languageText = "language server initializing…";
  $: treeFirst = Math.max(0, Math.floor(treeTop) - contract.overscan);
  $: treeLast = Math.min(files.length, Math.floor(treeTop) + contract.treeRows + contract.overscan);
  $: treeRows = files.slice(treeFirst, treeLast).map((path, offset) => ({ path, row: treeFirst + offset, name: path.split("/").at(-1)! }));
  $: editorFirst = Math.max(0, Math.floor(editorTop) - contract.overscan);
  $: editorLast = Math.min(lines.length, Math.floor(editorTop) + contract.editorRows + contract.overscan);
  $: editorRows = lines.slice(editorFirst, editorLast).map((source, offset) => ({ source, row: editorFirst + offset }));

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
  const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  async function openFile(path: string) {
    selectedFile = path; window.bench.telemetry({ event: "file_click", path });
    const text = await window.bench.read(path); lines = text.split("\n"); editorTop = 0;
    await tick(); await frame(); window.bench.telemetry({ event: "text_presented", path });
    await frame(); window.bench.telemetry({ event: "stable_frame", path });
  }
  async function scrollWorkload() {
    const events: Array<Record<string, unknown>> = []; let previous = performance.now(); let worstStall = 0;
    for (let leg = 0; leg < 4; leg += 1) for (let step = 0; step < 160; step += 1) {
      await frame(); const now = performance.now(); const durationMs = now - previous; previous = now;
      events.push({ event: "frame", durationMs, workload: "scroll" }); worstStall = Math.max(worstStall, durationMs - 16.7);
      const t = step / 159; const eased = (1 - Math.cos(Math.PI * t)) / 2; const forward = leg % 2 === 0;
      editorTop = (forward ? eased : 1 - eased) * 19970; treeTop = (forward ? eased : 1 - eased) * 5080;
      await tick();
    }
    events.push({ event: "main_thread_stall", durationMs: Math.max(0, worstStall), workload: "scroll" }); window.bench.telemetryBatch(events);
  }
  async function start() {
    await frame(); window.bench.telemetry({ event: "first_frame" }); window.bench.telemetry({ event: "project_open_requested" });
    files = await window.bench.scan(); scannerText = `${files.length.toLocaleString()} TypeScript files`;
    await tick(); await frame(); window.bench.telemetry({ event: "project_tree_visible" }); window.bench.telemetry({ event: "tree_presented" });
    await openFile("src/selected.ts"); window.bench.telemetry({ event: "lsp_request", method: "textDocument/documentSymbol" });
    symbols = await window.bench.symbols("src/selected.ts"); languageText = `${symbols.length.toLocaleString()} document symbols`;
    window.bench.telemetry({ event: "lsp_response", count: symbols.length }); await tick(); await frame(); window.bench.telemetry({ event: "outline_presented" });
    if (window.bench.config.autorun) {
      await scrollWorkload();
      for (let index = 0; index < 30; index += 1) await openFile(index % 2 ? "src/selected.ts" : "src/alternate.ts");
      window.bench.telemetry({ event: "benchmark_complete" });
    }
  }
  onMount(() => { void start(); });
</script>

<div class="shell">
  <aside class="activity"><div class="active">⌘</div><div>⌕</div><div>⑂</div><div>△</div></aside>
  <aside class="sidebar"><div class="section-title">EXPLORER · CONTROLLED FIXTURE</div><div class="tree"><div class="tree-content">
    {#each treeRows as item (item.path)}<div class:active={item.path === selectedFile} class="tree-row" data-path={item.path} style:transform={`translateY(${(item.row - treeTop) * contract.treeHeight}px)`}><span class="folder">◇</span>{item.name}</div>{/each}
  </div></div></aside>
  <main class="main"><div class="tabs"><div class="tab active"><span class="ts">TS</span><span class="tab-name">{selectedFile.split('/').at(-1)}</span><span>×</span></div><div class="tab"><span class="ts">TS</span>alternate.ts</div></div>
    <section class="editor"><div class="editor-content">{#each editorRows as item (item.row)}<div class="code-row" style:transform={`translateY(${(item.row - editorTop) * contract.lineHeight}px)`}><span class="line-no">{item.row + 1}</span><span class="code">{@html highlight(item.source)}</span></div>{/each}</div></section>
    <aside class="outline"><div class="section-title">OUTLINE</div><div class="outline-list">{#if symbols.length}{#each symbols.slice(0, 24) as symbol}<div class="outline-item"><span class="symbol">◇</span>{symbol.name}</div>{/each}{:else}<div class="outline-item">Initializing TypeScript…</div>{/if}</div></aside>
    <section class="output"><div class="output-tabs"><span class="active">OUTPUT</span><span>PROBLEMS</span><span>TERMINAL</span></div><div class="log">[benchmark] deterministic project fixture<br>[scanner] {scannerText}<br>[typescript] {languageText}</div></section>
  </main>
  <footer class="status"><div><span>⑂ benchmark/common</span><span>✓ 0</span><span>⚠ 0</span></div><div><span>Ln 1, Col 1</span><span>Spaces: 2</span><span>UTF-8</span><span>TypeScript</span></div></footer>
</div>
