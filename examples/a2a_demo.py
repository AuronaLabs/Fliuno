"""Run the real kernel against a local A2A 1.0 fixture, with no external API."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time

from rpc_client import LocalKernel, build_server


def fixture(cancel, streaming):
    class Handler(BaseHTTPRequestHandler):
        methods = []
        cancel_requested = threading.Event()

        def log_message(self, *_args):
            pass

        def reply(self, value):
            body = json.dumps(value, ensure_ascii=False).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def reply_event(self, request_id, result):
            envelope = {"jsonrpc": "2.0", "id": request_id, "result": result}
            body = json.dumps(envelope, ensure_ascii=False).encode("utf-8")
            self.wfile.write(b"data: " + body + b"\n\n")
            self.wfile.flush()

        def start_stream(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream; charset=utf-8")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

        def do_GET(self):
            assert self.path == "/.well-known/agent-card.json"
            self.reply({"supportedInterfaces": [{"url": f"http://{self.headers['Host']}/rpc",
                        "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
                        "capabilities": {"streaming": streaming}})

        def do_POST(self):
            assert self.path == "/rpc" and self.headers["A2A-Version"] == "1.0"
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            method = request["method"]
            Handler.methods.append(method)
            task = {"id": "demo-task", "contextId": "demo-context", "status": {"state": "TASK_STATE_WORKING"}}
            if method == "SendMessage":
                assert request["params"]["message"]["role"] == "ROLE_USER"
                assert request["params"]["configuration"]["returnImmediately"]
                result = {"task": task}
            elif method == "SendStreamingMessage":
                assert streaming
                assert request["params"]["message"]["role"] == "ROLE_USER"
                assert request["params"]["configuration"]["returnImmediately"]
                self.start_stream()
                self.reply_event(request["id"], {"task": task})
                if cancel:
                    if self.cancel_requested.wait(8):
                        self.reply_event(request["id"], {"statusUpdate": {
                            "taskId": task["id"], "contextId": task["contextId"],
                            "status": {"state": "TASK_STATE_CANCELED"}}})
                    return
                time.sleep(0.03)
                self.reply_event(request["id"], {"artifactUpdate": {
                    "taskId": task["id"], "contextId": task["contextId"],
                    "artifact": {"artifactId": "report", "name": "report.json",
                                  "parts": [{"text": "\u8fd9\u662f\u672c\u673a A2A fixture "}]},
                    "append": False, "lastChunk": False}})
                time.sleep(0.03)
                self.reply_event(request["id"], {"artifactUpdate": {
                    "taskId": task["id"], "contextId": task["contextId"],
                    "artifact": {"artifactId": "report", "name": "report.json",
                                  "parts": [{"text": "\u751f\u6210\u7684\u62a5\u544a\u3002"},
                                            {"data": {"rows": 3, "source": "local fixture"}},
                                            {"raw": "aGk=", "filename": "hi.txt", "mediaType": "text/plain"},
                                            {"url": f"http://{self.headers['Host']}/must-not-fetch"}]},
                    "append": True, "lastChunk": True}})
                time.sleep(0.03)
                self.reply_event(request["id"], {"statusUpdate": {
                    "taskId": task["id"], "contextId": task["contextId"],
                    "status": {"state": "TASK_STATE_COMPLETED"}}})
                return
            elif method == "GetTask":
                assert request["params"]["id"] == "demo-task"
                if not cancel:
                    task["status"]["state"] = "TASK_STATE_COMPLETED"
                    task["artifacts"] = [{"artifactId": "report", "name": "report.json", "parts": [
                        {"text": "这是本机 A2A fixture 生成的报告。"},
                        {"data": {"rows": 3, "source": "local fixture"}},
                        {"raw": "aGk=", "filename": "hi.txt", "mediaType": "text/plain"},
                        {"url": f"http://{self.headers['Host']}/must-not-fetch"},
                    ]}]
                result = task
            elif method == "CancelTask":
                assert request["params"]["id"] == "demo-task"
                if streaming:
                    self.cancel_requested.set()
                task["status"]["state"] = "TASK_STATE_CANCELED"
                result = task
            else:
                raise AssertionError(method)
            self.reply({"jsonrpc": "2.0", "id": request["id"], "result": result})

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cancel", action="store_true", help="Cancel after observing the remote identity")
    parser.add_argument("--streaming", action="store_true", help="Use A2A 1.0 SSE instead of polling")
    args = parser.parse_args()
    executable = build_server()
    server, thread, handler = fixture(args.cancel, args.streaming)
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="fliuno-a2a-demo-") as directory:
            directory = Path(directory)
            config = directory / "a2a.json"
            config.write_text(json.dumps({"agents": [{"id": "demo", "endpoint": base + "/rpc",
                "agentCardUrl": base + "/.well-known/agent-card.json", "pollIntervalMs": 50,
                "requestTimeoutMs": 3000, "taskTimeoutMs": 10000,
                "streaming": args.streaming}]}), encoding="utf-8")
            environment = dict(os.environ, FLIUNO_PROVIDER="demo")
            with LocalKernel(executable, ["--db", directory / "kernel.db", "--a2a-config", config], environment) as kernel:
                kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "a2a-demo"})
                workspace = kernel.call("workspace/open", {"root": str(directory)})
                session = kernel.call("session/create", {"workspaceId": workspace["id"]})
                task = kernel.call("task/start", {"sessionId": session["id"], "input": "生成报告",
                    "clientRequestId": "once", "backend": {"type": "external_agent", "agentId": "demo"}})
                if args.cancel:
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        observed = kernel.call("task/get", {"taskId": task["id"]})
                        if observed.get("agentRun", {}).get("remoteTask"):
                            break
                        time.sleep(0.01)
                    else:
                        raise TimeoutError("remote identity was not observed")
                    kernel.call("task/cancel", {"taskId": task["id"]})
                done = kernel.wait(task["id"], "cancelled" if args.cancel else "completed")
                # Local cancellation returns immediately. Wait for the bounded
                # remote acknowledgement rather than printing intent as proof.
                deadline = time.monotonic() + 5
                while not done["agentRun"]["finished"] and time.monotonic() < deadline:
                    time.sleep(0.01)
                    done = kernel.call("task/get", {"taskId": task["id"]})
                assert done["agentRun"]["finished"] and not done["agentRun"]["outcomeUnknown"]
                submission = "SendStreamingMessage" if args.streaming else "SendMessage"
                assert handler.methods.count(submission) == 1
                assert handler.methods.count("SendMessage") + handler.methods.count("SendStreamingMessage") == 1
                if args.streaming:
                    assert "GetTask" not in handler.methods
                elif not args.cancel:
                    assert "GetTask" in handler.methods
                assert handler.methods.count("CancelTask") == (1 if args.cancel else 0)
                artifacts = kernel.call("artifacts/list", {"taskId": task["id"]})
                assert len(artifacts) == (0 if args.cancel else 1)
                print("Result:", done["status"])
                print("Remote task:", json.dumps(done["agentRun"]["remoteTask"], ensure_ascii=False))
                print("Methods:", " -> ".join(handler.methods))
                for artifact in artifacts:
                    print("Artifact:", artifact["title"], artifact["content"])
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    main()
