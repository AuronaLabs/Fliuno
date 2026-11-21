"""Verify ACP against real OpenCode v1.19.6 with a local model endpoint."""
import argparse
from hashlib import file_digest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

from rpc_client import LocalKernel, build_server


VERSION = "1.19.6"
ANSWER = "opencode-acp-interop-ok"
READ = "read the fixture file"
CANCEL = "wait for local cancellation"
FILE_TEXT = "safe workspace fixture"


class Model(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        prompts = [item["content"] for item in request["messages"] if item["role"] == "user"]
        prompt = prompts[-1]
        title = any(item["role"] == "system" and str(item["content"]).startswith(
            "You are a title generator."
        ) for item in request["messages"])
        with self.server.calls_lock:
            (self.server.titles if title else self.server.calls).append(prompt)
        delta = {"role": "assistant", "content": ANSWER}
        finish_reason = "stop"
        if not title and prompt == READ:
            results = [m["content"] for m in request["messages"] if m["role"] == "tool"]
            if results:
                assert any(FILE_TEXT in str(result) for result in results), results
            else:
                assert any(t["function"]["name"] == "read" for t in request["tools"])
                delta = {"role": "assistant", "tool_calls": [{
                    "index": 0, "id": "fixture-read", "type": "function",
                    "function": {"name": "read", "arguments": json.dumps({
                        "filePath": str(self.server.workspace / "review.txt"),
                    })},
                }]}
                finish_reason = "tool_calls"
        if not title and prompt == CANCEL:
            self.server.cancel_input.set()
            self.server.cancel_release.wait(timeout=10)
            self.close_connection = True
            return
        chunks = []
        for update, stop in [(delta, None), ({}, finish_reason)]:
            chunk = {
                "id": "offline-stream", "object": "chat.completion.chunk", "created": 1,
                "model": request["model"],
                "choices": [{"index": 0, "delta": update, "finish_reason": stop}],
            }
            chunks.append(f"data: {json.dumps(chunk)}\n\n")
        body = ("".join(chunks) + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def test_environment(root, port):
    settings = {}
    for name in (
        "APPDATA", "LOCALAPPDATA", "USERPROFILE", "HOME", "XDG_DATA_HOME",
        "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
    ):
        path = root / name
        path.mkdir()
        settings[name] = str(path)
    for name in (
        "AUTOUPDATE", "MODELS_FETCH", "DEFAULT_PLUGINS", "LSP_DOWNLOAD",
        "PROJECT_CONFIG", "CLAUDE_CODE", "SHARE",
    ):
        settings[f"OPENCODE_DISABLE_{name}"] = "1"
    settings["OPENCODE_CONFIG_CONTENT"] = json.dumps({
        "autoupdate": False, "share": "disabled", "permission": {"*": "deny", "read": "ask"},
        "model": "fixture/local-test-model",
        "provider": {"fixture": {
            "npm": "@ai-sdk/openai-compatible", "name": "Local Fixture",
            "options": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "fixture-only"},
            "models": {"local-test-model": {
                "name": "Local Test Model", "limit": {"context": 4096, "output": 1024},
            }},
        }},
    })
    return settings


def check(app, executable, listener, root):
    workspace = root / "workspace"
    workspace.mkdir()
    listener.workspace = workspace
    (workspace / "review.txt").write_text(FILE_TEXT, encoding="utf-8")
    settings = test_environment(root, listener.server_port)
    env = dict(os.environ, **settings)
    env["FLIUNO_PROVIDER"] = "demo"
    actual = subprocess.check_output(
        [str(executable), "--version"], cwd=workspace, env=env, text=True, timeout=15,
    ).strip()
    assert actual == VERSION, f"expected OpenCode {VERSION}, found {actual}"
    config = root / "acp.json"
    config.write_text(json.dumps({"agents": [{
        "id": "opencode", "command": str(executable), "args": ["acp"],
        "isolation": "trusted_host", "cwd": str(workspace),
        "authenticationMethod": "opencode-login", "envAllow": list(settings),
        "promptTimeoutMs": 8000,
    }]}), encoding="utf-8")
    source = root / "kernel.db"
    args = ["--db", source, "--acp-config", config]
    init = {"protocolVersion": "0.1", "clientName": "opencode-acp-interop"}

    def check_turn(kernel, started):
        done = kernel.wait(started["id"], "completed")
        assert done["agentRun"]["authentication"] == "succeeded"
        assert done["output"] == ANSWER, done
        return done["agentRun"]["sessionId"]

    with LocalKernel(app, args, env) as kernel:
        caps = kernel.call("initialize", init)["capabilities"]
        assert caps["agentAuthenticationState"] and caps["agentSessionContinuation"]
        record = kernel.call("workspace/open", {"root": str(workspace)})
        session = kernel.call("session/create", {"workspaceId": record["id"]})
        request = {
            "sessionId": session["id"], "input": "first ACP turn", "clientRequestId": "first",
            "backend": {"type": "external_agent", "agentId": "opencode"},
        }
        first = kernel.call("task/start", request)
        agent_session = check_turn(kernel, first)

    def digest(path):
        with path.open("rb") as data:
            return file_digest(data, "sha256").hexdigest()

    source_digest = digest(source)
    backup = root / "backup"
    restored = root / "restored.db"
    for operation, *paths in [
        ("create", source, backup), ("restore", backup, restored),
        ("verify-restored", backup, restored),
    ]:
        result = subprocess.check_output(
            [str(app), "backup", operation, *map(str, paths)], env=env, text=True, timeout=15,
        )
        assert json.loads(result)["operation"] == operation, result
    args = ["--db", restored, "--acp-config", config]

    with LocalKernel(app, args, env) as kernel:
        kernel.call("initialize", init)
        assert kernel.call("task/start", request)["id"] == first["id"]
        continuation = {
            "taskId": first["id"], "input": "second ACP turn", "clientRequestId": "second",
        }
        second = kernel.call("task/continueAgentSession", continuation)
        assert check_turn(kernel, second) == agent_session
        assert kernel.call("task/continueAgentSession", continuation)["id"] == second["id"]

        read = kernel.call("task/continueAgentSession", {
            "taskId": second["id"], "input": READ, "clientRequestId": "read",
        })
        waiting = kernel.wait(read["id"], "waiting_approval")
        approvals = waiting["agentRun"]["pendingApprovals"]
        assert len(approvals) == 1 and approvals[0]["allowOnceAvailable"], approvals
        call = approvals[0]["request"]["toolCall"]
        assert call["toolCallId"] == "fixture-read" and call["kind"] == "read", call
        kernel.call("approval/respond", {"taskId": read["id"], "approvalId": approvals[0]["id"],
                                        "decision": "allow_once"})
        assert check_turn(kernel, read) == agent_session

        stop = {"taskId": read["id"], "input": CANCEL, "clientRequestId": "stop"}
        cancelled = kernel.call("task/continueAgentSession", stop)
        try:
            assert listener.cancel_input.wait(timeout=7), "cancel fixture was not reached"
            kernel.call("task/cancel", {"taskId": cancelled["id"]})
            done = kernel.wait(cancelled["id"], "cancelled")
            assert done["agentRun"]["authentication"] == "succeeded"
            deadline = time.monotonic() + 5
            while not done["agentRun"]["finished"]:
                assert time.monotonic() < deadline, "cancelled peer did not close"
                time.sleep(0.02)
                done = kernel.call("task/get", {"taskId": cancelled["id"]})
        finally:
            listener.cancel_release.set()

    with LocalKernel(app, args, env) as kernel:
        kernel.call("initialize", init)
        assert kernel.call("task/continueAgentSession", stop)["id"] == cancelled["id"]

    with listener.calls_lock:
        assert listener.calls == ["first ACP turn", "second ACP turn", READ, READ, CANCEL], listener.calls
        assert listener.titles == ["first ACP turn"], listener.titles
    assert digest(source) == source_digest
    print(f"OpenCode {VERSION}: protocol login, backup cutover, session resume, approval, cancellation, and receipt replay passed.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--opencode", type=Path, required=True)
    parser.add_argument("--app-server", type=Path)
    opts = parser.parse_args()
    executable = opts.opencode.resolve(strict=True)
    app = opts.app_server.resolve(strict=True) if opts.app_server else build_server()
    with tempfile.TemporaryDirectory(prefix="fliuno-opencode-") as temp, \
            ThreadingHTTPServer(("127.0.0.1", 0), Model) as listener:
        listener.calls = []
        listener.titles = []
        listener.calls_lock = threading.Lock()
        listener.cancel_input = threading.Event()
        listener.cancel_release = threading.Event()
        worker = threading.Thread(target=listener.serve_forever)
        worker.start()
        try:
            check(app, executable, listener, Path(temp))
        finally:
            listener.shutdown()
            worker.join()


if __name__ == "__main__":
    main()
