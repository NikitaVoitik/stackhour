package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	wailsruntime "github.com/wailsapp/wails/v2/pkg/runtime"
)

type Config struct {
	Candidate string `json:"candidate"`
	Autorun   bool   `json:"autorun"`
}

type Symbol struct {
	Name string `json:"name"`
	Kind int    `json:"kind"`
}

type App struct {
	ctx        context.Context
	started    time.Time
	phase      string
	runID      string
	logPath    string
	fixture    string
	lspCommand string
	logMu      sync.Mutex
	lspMu      sync.Mutex
	lsp        *languageServer
}

func newApp() *App {
	fixture := os.Getenv("BENCH_FIXTURE")
	if fixture == "" {
		fixture = ".fixture"
	}
	lspCommand := os.Getenv("BENCH_LSP")
	if lspCommand == "" {
		lspCommand = "typescript-language-server"
	}
	phase := os.Getenv("BENCH_PHASE")
	if phase == "" {
		phase = "visual"
	}
	runID := os.Getenv("BENCH_RUN_ID")
	if runID == "" {
		runID = "manual"
	}
	return &App{
		started:    time.Now(),
		phase:      phase,
		runID:      runID,
		logPath:    os.Getenv("BENCH_LOG"),
		fixture:    fixture,
		lspCommand: lspCommand,
	}
}

func (a *App) startup(ctx context.Context) {
	a.ctx = ctx
	a.emit(map[string]interface{}{"event": "process_start"})
}

func (a *App) shutdown(context.Context) {
	a.lspMu.Lock()
	defer a.lspMu.Unlock()
	if a.lsp != nil {
		a.lsp.close()
		a.lsp = nil
	}
}

func (a *App) Config() Config {
	return Config{Candidate: "wails", Autorun: os.Getenv("BENCH_AUTORUN") == "1"}
}

func (a *App) Scan() ([]string, error) {
	var files []string
	err := filepath.WalkDir(a.fixture, func(path string, entry os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if !entry.IsDir() && strings.EqualFold(filepath.Ext(path), ".ts") {
			relative, relativeErr := filepath.Rel(a.fixture, path)
			if relativeErr != nil {
				return relativeErr
			}
			files = append(files, filepath.ToSlash(relative))
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	sort.Strings(files)
	a.emit(map[string]interface{}{"event": "project_scan_complete", "count": len(files)})
	return files, nil
}

func (a *App) Read(path string) (string, error) {
	data, err := os.ReadFile(filepath.Join(a.fixture, filepath.FromSlash(path)))
	if err != nil {
		return "", err
	}
	a.emit(map[string]interface{}{
		"event": "disk_read_complete",
		"path":  path,
		"bytes": len(data),
	})
	return string(data), nil
}

func (a *App) Symbols(path string) ([]Symbol, error) {
	a.lspMu.Lock()
	defer a.lspMu.Unlock()
	if a.lsp == nil {
		server, err := startLanguageServer(a.lspCommand, a.fixture)
		if err != nil {
			return nil, err
		}
		a.lsp = server
	}
	return a.lsp.documentSymbols(path)
}

func (a *App) Telemetry(event map[string]interface{}) {
	a.emit(event)
	switch event["event"] {
	case "first_frame":
		a.memory("idle")
	case "outline_presented":
		a.memory("loaded")
	case "benchmark_complete":
		a.memory("complete")
		go func() {
			time.Sleep(50 * time.Millisecond)
			wailsruntime.Quit(a.ctx)
		}()
	}
}

func (a *App) TelemetryBatch(events []map[string]interface{}) {
	for _, event := range events {
		a.emit(event)
	}
}

func (a *App) emit(event map[string]interface{}) {
	a.logMu.Lock()
	defer a.logMu.Unlock()
	row := map[string]interface{}{
		"schemaVersion": 1,
		"candidate":     "wails",
		"phase":         a.phase,
		"runId":         a.runID,
		"timestampNs":   time.Since(a.started).Nanoseconds(),
	}
	for key, value := range event {
		row[key] = value
	}
	data, err := json.Marshal(row)
	if err != nil {
		return
	}
	data = append(data, '\n')
	if a.logPath == "" {
		_, _ = os.Stdout.Write(data)
		return
	}
	file, err := os.OpenFile(a.logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err == nil {
		_, _ = file.Write(data)
		_ = file.Close()
	}
}

func (a *App) memory(label string) {
	a.emit(map[string]interface{}{
		"event":     "memory_sample",
		"label":     label,
		"processes": processMemory(os.Getpid()),
	})
}

type languageServer struct {
	command *exec.Cmd
	input   io.WriteCloser
	output  *bufio.Reader
	nextID  int
	root    string
}

func startLanguageServer(command, root string) (*languageServer, error) {
	process := exec.Command(command, "--stdio")
	process.Dir = root
	process.Stderr = io.Discard
	input, err := process.StdinPipe()
	if err != nil {
		return nil, err
	}
	output, err := process.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := process.Start(); err != nil {
		return nil, err
	}
	server := &languageServer{
		command: process,
		input:   input,
		output:  bufio.NewReader(output),
		nextID:  1,
		root:    root,
	}
	rootPath, _ := filepath.Abs(root)
	rootURI := fileURI(rootPath)
	_, err = server.request("initialize", map[string]interface{}{
		"processId": os.Getpid(),
		"rootUri":   rootURI,
		"capabilities": map[string]interface{}{
			"textDocument": map[string]interface{}{
				"documentSymbol": map[string]interface{}{"hierarchicalDocumentSymbolSupport": true},
			},
		},
		"workspaceFolders": []map[string]interface{}{{"uri": rootURI, "name": "controlled-fixture"}},
	})
	if err != nil {
		server.close()
		return nil, err
	}
	if err := server.notify("initialized", map[string]interface{}{}); err != nil {
		server.close()
		return nil, err
	}
	return server, nil
}

func (s *languageServer) documentSymbols(relative string) ([]Symbol, error) {
	absolute := filepath.Join(s.root, filepath.FromSlash(relative))
	source, err := os.ReadFile(absolute)
	if err != nil {
		return nil, err
	}
	absolute, _ = filepath.Abs(absolute)
	uri := fileURI(absolute)
	if err := s.notify("textDocument/didOpen", map[string]interface{}{
		"textDocument": map[string]interface{}{
			"uri": uri, "languageId": "typescript", "version": 1, "text": string(source),
		},
	}); err != nil {
		return nil, err
	}
	response, err := s.request("textDocument/documentSymbol", map[string]interface{}{
		"textDocument": map[string]interface{}{"uri": uri},
	})
	if err != nil {
		return nil, err
	}
	data, err := json.Marshal(response["result"])
	if err != nil {
		return nil, err
	}
	var symbols []Symbol
	if err := json.Unmarshal(data, &symbols); err != nil {
		return nil, err
	}
	return symbols, nil
}

func (s *languageServer) notify(method string, params interface{}) error {
	return s.write(map[string]interface{}{
		"jsonrpc": "2.0",
		"method":  method,
		"params":  params,
	})
}

func (s *languageServer) request(method string, params interface{}) (map[string]interface{}, error) {
	id := s.nextID
	s.nextID++
	if err := s.write(map[string]interface{}{
		"jsonrpc": "2.0",
		"id":      id,
		"method":  method,
		"params":  params,
	}); err != nil {
		return nil, err
	}
	for {
		message, err := s.read()
		if err != nil {
			return nil, err
		}
		if messageID, ok := message["id"].(float64); ok && int(messageID) == id {
			return message, nil
		}
	}
}

func (s *languageServer) write(message map[string]interface{}) error {
	body, err := json.Marshal(message)
	if err != nil {
		return err
	}
	if _, err := fmt.Fprintf(s.input, "Content-Length: %d\r\n\r\n", len(body)); err != nil {
		return err
	}
	_, err = s.input.Write(body)
	return err
}

func (s *languageServer) read() (map[string]interface{}, error) {
	length := -1
	for {
		header, err := s.output.ReadString('\n')
		if err != nil {
			return nil, err
		}
		if header == "\r\n" {
			break
		}
		if strings.HasPrefix(strings.ToLower(header), "content-length:") {
			parts := strings.SplitN(header, ":", 2)
			if len(parts) == 2 {
				parsed, parseErr := strconv.Atoi(strings.TrimSpace(parts[1]))
				if parseErr != nil {
					return nil, fmt.Errorf("invalid language server content length: %w", parseErr)
				}
				length = parsed
			}
		}
	}
	if length < 0 {
		return nil, errors.New("language server response missing content length")
	}
	body := make([]byte, length)
	if _, err := io.ReadFull(s.output, body); err != nil {
		return nil, err
	}
	var message map[string]interface{}
	if err := json.Unmarshal(body, &message); err != nil {
		return nil, err
	}
	return message, nil
}

func (s *languageServer) close() {
	if s.command != nil && s.command.Process != nil {
		_ = s.command.Process.Kill()
		_, _ = s.command.Process.Wait()
	}
}

func fileURI(path string) string {
	return "file://" + filepath.ToSlash(path)
}

func processMemory(root int) []map[string]interface{} {
	if runtime.GOOS != "linux" {
		return []map[string]interface{}{}
	}
	pids := descendants(root)
	rows := make([]map[string]interface{}, 0, len(pids))
	for _, pid := range pids {
		status, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
		if err != nil {
			continue
		}
		cmdline, _ := os.ReadFile(fmt.Sprintf("/proc/%d/cmdline", pid))
		command := strings.ReplaceAll(string(cmdline), "\x00", " ")
		rss := 0
		for _, line := range strings.Split(string(status), "\n") {
			if strings.HasPrefix(line, "VmRSS:") {
				fields := strings.Fields(line)
				if len(fields) > 1 {
					rss, _ = strconv.Atoi(fields[1])
				}
				break
			}
		}
		group := "ui-runtime"
		if strings.Contains(command, "typescript-language-server") {
			group = "typescript-language-server"
		} else if strings.Contains(command, "tsserver") {
			group = "tsserver"
		}
		rows = append(rows, map[string]interface{}{"pid": pid, "rssKb": rss, "group": group})
	}
	return rows
}

func descendants(root int) []int {
	pids := []int{root}
	known := map[int]bool{root: true}
	changed := true
	for changed {
		changed = false
		entries, err := os.ReadDir("/proc")
		if err != nil {
			break
		}
		for _, entry := range entries {
			pid, err := strconv.Atoi(entry.Name())
			if err != nil || known[pid] {
				continue
			}
			status, err := os.ReadFile(filepath.Join("/proc", entry.Name(), "status"))
			if err != nil {
				continue
			}
			parent := 0
			for _, line := range strings.Split(string(status), "\n") {
				if strings.HasPrefix(line, "PPid:") {
					fields := strings.Fields(line)
					if len(fields) > 1 {
						parent, _ = strconv.Atoi(fields[1])
					}
					break
				}
			}
			if known[parent] {
				known[pid] = true
				pids = append(pids, pid)
				changed = true
			}
		}
	}
	return pids
}
