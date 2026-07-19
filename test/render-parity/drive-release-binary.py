#!/usr/bin/env python3
"""Drive the RELEASE stackhour coordinator against a local mock Bot API and
print the payloads it produced for one engine output.

SAFETY: the token below is fake and `apiRoot` points at 127.0.0.1, so the
process cannot reach api.telegram.org. The owner's live coordinator and its
token are never touched.

Usage: drive-release-binary.py <case-name>
"""
import http.server, json, os, pathlib, subprocess, sys, tempfile, threading, time

REPO = pathlib.Path(__file__).resolve().parents[2]
BIN = REPO / "target/release/stackhour"
CASES = json.loads((REPO / "test/render-parity/cases.json").read_text())

case_name = sys.argv[1] if len(sys.argv) > 1 else "markdown-table"
case = next(c for c in CASES if c["name"] == case_name)

recorded = []
served_update = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        method = self.path.rsplit("/", 1)[-1]
        if method == "getUpdates":
            if served_update.is_set():
                time.sleep(0.2)
                return self.reply({"ok": True, "result": []})
            served_update.set()
            return self.reply({"ok": True, "result": [{
                "update_id": 1,
                "message": {"message_id": 10, "date": int(time.time()),
                            "chat": {"id": 4242, "type": "private"},
                            "from": {"id": 4242, "is_bot": False, "first_name": "N"},
                            "text": case["text"]},
            }]})
        recorded.append({"method": method, "body": body})
        if method == "sendRichMessage":
            return self.reply({"ok": False, "error_code": 404,
                               "description": "Not Found: method not found"}, 404)
        self.reply({"ok": True, "result": {"message_id": len(recorded) + 100}})

    def reply(self, obj, status=200):
        raw = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

home = pathlib.Path(tempfile.mkdtemp(prefix="stackhour-parity-"))
(home / "engines").mkdir(parents=True)
# `cat` echoes the prompt straight back, so the engine's "answer" is exactly
# the case text and the only thing under test is the rendering.
(home / "engines/claude.toml").write_text(
    'label = "Echo"\nbin = "/bin/cat"\nkind = "plain-lines"\nargs = []\n'
)
(home / "config.json").write_text(json.dumps({
    "token": "0000000000:FAKE-LOCAL-MOCK-TOKEN-NOT-A-SECRET",
    "chatId": 4242,
    "defaultTarget": "gcp",
    "apiRoot": f"http://127.0.0.1:{port}",
    "targets": {"gcp": {"label": "GCP", "type": "local", "cwd": str(home),
                        "claudeBin": "/bin/cat", "permissionMode": "default"}},
}))

env = dict(os.environ, STACKHOUR_BRIDGE_HOME=str(home))
proc = subprocess.Popen([str(BIN), "bridge", "coordinator", "--runtime-dir", str(home)],
                        env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
deadline = time.time() + 30
while time.time() < deadline:
    if any(r["method"] == "sendMessage" and r["body"].get("reply_markup") for r in recorded):
        time.sleep(0.4)
        break
    time.sleep(0.2)
proc.terminate()
try:
    out = proc.communicate(timeout=10)[0]
except subprocess.TimeoutExpired:
    proc.kill()
    out = proc.communicate()[0]

print(json.dumps({"case": case_name, "calls": recorded}, indent=2, ensure_ascii=False))
if not recorded:
    sys.stderr.write("no payloads recorded; coordinator output:\n" + (out or "")[-4000:] + "\n")
    sys.exit(1)
