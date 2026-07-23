import { useEffect, useMemo, useState } from "react";
import { flushSync } from "react-dom";

declare global {
  interface Window {
    bench: {
      scan(): Promise<string[]>;
      read(path: string): Promise<string>;
      symbols(path: string): Promise<Array<{ name: string; kind: number }>>;
      telemetry(event: Record<string, unknown>): void;
      telemetryBatch(events: Array<Record<string, unknown>>): void;
      config: { candidate: string; autorun: boolean };
    };
  }
}

const contract = {
  treeRows: 40,
  editorRows: 30,
  overscan: 4,
  lineHeight: 20,
  treeHeight: 18,
};

const escape = (value: string) =>
  value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");

function highlight(source: string) {
  if (source.trimStart().startsWith("//")) {
    return `<span class="comment">${escape(source)}</span>`;
  }
  return escape(source)
    .replace(/(&quot;[^&]*?&quot;|"[^"\n]*")/g, '<span class="str">$1</span>')
    .replace(
      /\b(export|interface|const|function|return|type|void|null|boolean|string|number)\b/g,
      '<span class="kw">$1</span>',
    )
    .replace(/\b(\d+)\b/g, '<span class="num">$1</span>')
    .replace(/\b(Item\d+|Result\d+|Record\d+)\b/g, '<span class="type">$1</span>')
    .replace(/\b(format\d+)\b/g, '<span class="fn">$1</span>');
}

const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

export default function App() {
  const [files, setFiles] = useState<string[]>([]);
  const [lines, setLines] = useState<string[]>([]);
  const [selectedFile, setSelectedFile] = useState("src/selected.ts");
  const [treeTop, setTreeTop] = useState(0);
  const [editorTop, setEditorTop] = useState(0);
  const [symbols, setSymbols] = useState<Array<{ name: string; kind: number }>>([]);
  const [scannerText, setScannerText] = useState("waiting…");
  const [languageText, setLanguageText] = useState("language server initializing…");

  const treeRows = useMemo(() => {
    const first = Math.max(0, Math.floor(treeTop) - contract.overscan);
    const last = Math.min(files.length, Math.floor(treeTop) + contract.treeRows + contract.overscan);
    return files.slice(first, last).map((path, offset) => ({
      path,
      row: first + offset,
      name: path.split("/").at(-1)!,
    }));
  }, [files, treeTop]);

  const editorRows = useMemo(() => {
    const first = Math.max(0, Math.floor(editorTop) - contract.overscan);
    const last = Math.min(lines.length, Math.floor(editorTop) + contract.editorRows + contract.overscan);
    return lines.slice(first, last).map((source, offset) => ({
      source,
      row: first + offset,
    }));
  }, [editorTop, lines]);

  useEffect(() => {
    async function openFile(path: string) {
      flushSync(() => setSelectedFile(path));
      window.bench.telemetry({ event: "file_click", path });
      const text = await window.bench.read(path);
      flushSync(() => {
        setLines(text.split("\n"));
        setEditorTop(0);
      });
      await frame();
      window.bench.telemetry({ event: "text_presented", path });
      await frame();
      window.bench.telemetry({ event: "stable_frame", path });
    }

    async function scrollWorkload() {
      const events: Array<Record<string, unknown>> = [];
      let previous = performance.now();
      let worstStall = 0;
      for (let leg = 0; leg < 4; leg += 1) {
        for (let step = 0; step < 160; step += 1) {
          await frame();
          const now = performance.now();
          const durationMs = now - previous;
          previous = now;
          events.push({ event: "frame", durationMs, workload: "scroll" });
          worstStall = Math.max(worstStall, durationMs - 16.7);
          const t = step / 159;
          const eased = (1 - Math.cos(Math.PI * t)) / 2;
          const forward = leg % 2 === 0;
          flushSync(() => {
            setEditorTop((forward ? eased : 1 - eased) * 19970);
            setTreeTop((forward ? eased : 1 - eased) * 5080);
          });
        }
      }
      events.push({
        event: "main_thread_stall",
        durationMs: Math.max(0, worstStall),
        workload: "scroll",
      });
      window.bench.telemetryBatch(events);
    }

    async function start() {
      await frame();
      window.bench.telemetry({ event: "first_frame" });
      window.bench.telemetry({ event: "project_open_requested" });
      const scannedFiles = await window.bench.scan();
      flushSync(() => {
        setFiles(scannedFiles);
        setScannerText(`${scannedFiles.length.toLocaleString()} TypeScript files`);
      });
      await frame();
      window.bench.telemetry({ event: "project_tree_visible" });
      window.bench.telemetry({ event: "tree_presented" });
      await openFile("src/selected.ts");
      window.bench.telemetry({ event: "lsp_request", method: "textDocument/documentSymbol" });
      const documentSymbols = await window.bench.symbols("src/selected.ts");
      flushSync(() => {
        setSymbols(documentSymbols);
        setLanguageText(`${documentSymbols.length.toLocaleString()} document symbols`);
      });
      window.bench.telemetry({ event: "lsp_response", count: documentSymbols.length });
      await frame();
      window.bench.telemetry({ event: "outline_presented" });
      if (window.bench.config.autorun) {
        await scrollWorkload();
        for (let index = 0; index < 30; index += 1) {
          await openFile(index % 2 ? "src/selected.ts" : "src/alternate.ts");
        }
        window.bench.telemetry({ event: "benchmark_complete" });
      }
    }

    void start();
  }, []);

  return (
    <div className="shell">
      <aside className="activity">
        <div className="active">⌘</div><div>⌕</div><div>⑂</div><div>△</div>
      </aside>
      <aside className="sidebar">
        <div className="section-title">EXPLORER · CONTROLLED FIXTURE</div>
        <div className="tree"><div className="tree-content">
          {treeRows.map((item) => (
            <div
              className={`tree-row${item.path === selectedFile ? " active" : ""}`}
              data-path={item.path}
              key={item.path}
              style={{ transform: `translateY(${(item.row - treeTop) * contract.treeHeight}px)` }}
            >
              <span className="folder">◇</span>{item.name}
            </div>
          ))}
        </div></div>
      </aside>
      <main className="main">
        <div className="tabs">
          <div className="tab active"><span className="ts">TS</span><span className="tab-name">{selectedFile.split("/").at(-1)}</span><span>×</span></div>
          <div className="tab"><span className="ts">TS</span>alternate.ts</div>
        </div>
        <section className="editor"><div className="editor-content">
          {editorRows.map((item) => (
            <div
              className="code-row"
              key={item.row}
              style={{ transform: `translateY(${(item.row - editorTop) * contract.lineHeight}px)` }}
            >
              <span className="line-no">{item.row + 1}</span>
              <span className="code" dangerouslySetInnerHTML={{ __html: highlight(item.source) }} />
            </div>
          ))}
        </div></section>
        <aside className="outline">
          <div className="section-title">OUTLINE</div>
          <div className="outline-list">
            {symbols.length
              ? symbols.slice(0, 24).map((symbol, index) => (
                  <div className="outline-item" key={`${symbol.name}-${index}`}><span className="symbol">◇</span>{symbol.name}</div>
                ))
              : <div className="outline-item">Initializing TypeScript…</div>}
          </div>
        </aside>
        <section className="output">
          <div className="output-tabs"><span className="active">OUTPUT</span><span>PROBLEMS</span><span>TERMINAL</span></div>
          <div className="log">[benchmark] deterministic project fixture<br />[scanner] {scannerText}<br />[typescript] {languageText}</div>
        </section>
      </main>
      <footer className="status">
        <div><span>⑂ benchmark/common</span><span>✓ 0</span><span>⚠ 0</span></div>
        <div><span>Ln 1, Col 1</span><span>Spaces: 2</span><span>UTF-8</span><span>TypeScript</span></div>
      </footer>
    </div>
  );
}
