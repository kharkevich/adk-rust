"""A scripted language server for the LanguageServerPool tests.

Modes (argv[1]):
  normal            versioned diagnostics on open and change: one error per line containing
                    ERROR; a save runs a "check" (progress begin and end) that also reports
                    an error in other.rs when the document contains BREAK_OTHER
  crash-after-init  answers initialize, then exits
  never-answers     reads and ignores everything

argv[2], when given, is a file the server writes its pid and its child's pid to.
"""

import json
import os
import subprocess
import sys
import time

MODE = sys.argv[1] if len(sys.argv) > 1 else "normal"
PIDS = sys.argv[2] if len(sys.argv) > 2 else None
stdin = sys.stdin.buffer
stdout = sys.stdout.buffer


def read():
    length = 0
    while True:
        line = stdin.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            length = int(value.strip())
    return json.loads(stdin.read(length))


def send(message):
    body = json.dumps(message).encode()
    stdout.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    stdout.flush()


def publish(uri, version, text):
    diagnostics = []
    for number, line in enumerate(text.split("\n")):
        if "ERROR" in line:
            diagnostics.append({
                "range": {"start": {"line": number, "character": line.index("ERROR")},
                          "end": {"line": number, "character": line.index("ERROR") + 5}},
                "severity": 1,
                "code": "E1",
                "message": "found ERROR",
            })
        if "WARN" in line:
            diagnostics.append({
                "range": {"start": {"line": number, "character": 0}, "end": {"line": number, "character": 1}},
                "severity": 2,
                "message": "a warning",
            })
    params = {"uri": uri, "diagnostics": diagnostics}
    if version is not None:
        params["version"] = version
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params})


if PIDS:
    child = subprocess.Popen(["sleep", "300"])
    with open(PIDS, "w") as out:
        out.write("%d %d\n" % (os.getpid(), child.pid))

docs = {}
while True:
    message = read()
    method = message.get("method")
    if MODE == "never-answers":
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {
            "textDocumentSync": {"openClose": True, "change": 1, "save": {}},
            "definitionProvider": True,
        }}})
        if MODE == "crash-after-init":
            time.sleep(0.05)
            sys.exit(3)
    elif method == "textDocument/didOpen":
        doc = message["params"]["textDocument"]
        docs[doc["uri"]] = doc["text"]
        publish(doc["uri"], doc["version"], doc["text"])
    elif method == "textDocument/didChange":
        params = message["params"]
        uri = params["textDocument"]["uri"]
        docs[uri] = params["contentChanges"][-1]["text"]
        publish(uri, params["textDocument"]["version"], docs[uri])
    elif method == "textDocument/didSave":
        uri = message["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": 900, "method": "window/workDoneProgress/create", "params": {"token": "check"}})
        send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "check", "value": {"kind": "begin", "title": "check"}}})
        time.sleep(0.1)
        other = uri.rsplit("/", 1)[0] + "/other.rs"
        publish(other, None, "ERROR from the check" if "BREAK_OTHER" in docs.get(uri, "") else "")
        send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "check", "value": {"kind": "end"}}})
    elif method == "textDocument/definition":
        uri = message["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": [
            {"uri": uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}}]})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        sys.exit(0)
    elif "id" in message and method is not None:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "unknown"}})
