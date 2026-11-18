"""Offline remote-tool demo: HTTP discovery -> consent -> call -> durable result.

Both the MCP server and model are explicit loopback fixtures. No external API
calls, real credentials or host workspace writes are involved.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sys
import tempfile
import threading

from rpc_client import LocalKernel, build_server


def fixture(legacy, sse):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        calls = 0
        methods = []

        def log_message(self, *_args):
            pass

        def reply(self, status, value, headers=None, stream=False):
            body = json.dumps(value, ensure_ascii=False)
            if stream:
                body = f"event: message\ndata: {body}\n\n"
            body = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

        def check_mcp_auth(self):
            assert self.headers["Authorization"] == "Bearer fixture-mcp-token"
            assert self.headers["X-Vendor-Key"] == "fixture-vendor-key"

        def do_GET(self):
            self.check_mcp_auth()
            self.reply(405, None)

        def do_DELETE(self):
            self.check_mcp_auth()
            self.reply(200, None)

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            assert 0 < length <= 1024 * 1024
            request = json.loads(self.rfile.read(length))
            if self.path == "/v1/responses":
                assert self.headers["Authorization"] == "Bearer fixture-model-key"
                assert "X-Vendor-Key" not in self.headers
                previous = next((v for v in request["input"] if v.get("type") == "function_call_output"), None)
                if previous:
                    assert "你好，远程工具" in previous["output"]
                    output = [{"type": "message", "content": [{
                        "type": "output_text", "text": "远程工具结果已保存。",
                    }]}]
                else:
                    name = next(t["name"] for t in request["tools"] if t["name"].startswith("mcp_remote_"))
                    output = [{"type": "function_call", "call_id": "remote-call", "name": name,
                               "arguments": json.dumps({"text": "你好，远程工具"}, ensure_ascii=False)}]
                self.reply(200, {"status": "completed", "output": output})
                return
            assert self.path == "/mcp"
            self.check_mcp_auth()
            method = request["method"]
            Handler.methods.append(method)
            response = {"jsonrpc": "2.0", "id": request.get("id")}
            headers = {}
            if method == "server/discover":
                if legacy:
                    response["error"] = {"code": -32601, "message": "Legacy fixture"}
                    self.reply(200, response)
                    return
                result = {"resultType": "complete", "supportedVersions": ["2026-07-28"],
                          "capabilities": {"tools": {}}, "ttlMs": 0, "cacheScope": "private"}
            elif method == "initialize":
                assert legacy
                result = {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                          "serverInfo": {"name": "demo-fixture", "version": "0.1"}}
                headers["Mcp-Session-Id"] = "fixture-session"
            elif method in {"notifications/initialized", "notifications/cancelled"}:
                self.reply(202, None)
                return
            elif method == "tools/list":
                result = {"tools": [{"name": "echo", "description": "Loopback fixture only",
                                     "inputSchema": {"type": "object", "required": ["text"],
                                                     "properties": {"text": {"type": "string"}},
                                                     "additionalProperties": False}}]}
            elif method == "tools/call":
                Handler.calls += 1
                result = {"content": [{"type": "text", "text": request["params"]["arguments"]["text"]}]}
            else:
                raise AssertionError(method)
            response["result"] = result
            self.reply(200, response, headers, stream=sse and method == "tools/call")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deny", action="store_true")
    parser.add_argument("--legacy", action="store_true", help="Use the 2025-11-25 lifecycle fixture")
    parser.add_argument("--sse", action="store_true", help="Return the tool result as SSE")
    args = parser.parse_args()
    executable = build_server()
    server, thread, handler = fixture(args.legacy, args.sse)
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="fliuno-http-demo-") as directory:
            directory = Path(directory)
            config = directory / "mcp.json"
            config.write_text(json.dumps({"servers": [{
                "id": "remote", "transport": "streamable_http", "url": base + "/mcp",
                "bearerTokenEnv": "FLIUNO_DEMO_MCP_TOKEN",
                "headersEnv": {"X-Vendor-Key": "FLIUNO_DEMO_VENDOR_KEY"}, "timeoutMs": 5000,
            }]}), encoding="utf-8")
            environment = dict(os.environ, FLIUNO_PROVIDER="openai", FLIUNO_MODEL="fixture-model",
                               OPENAI_BASE_URL=base + "/v1/", OPENAI_API_KEY="fixture-model-key",
                               FLIUNO_DEMO_MCP_TOKEN="fixture-mcp-token",
                               FLIUNO_DEMO_VENDOR_KEY="fixture-vendor-key")
            with LocalKernel(executable, ["--db", directory / "kernel.db", "--mcp-config", config], environment) as kernel:
                kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "http-demo"})
                workspace = kernel.call("workspace/open", {"root": str(directory)})
                session = kernel.call("session/create", {"workspaceId": workspace["id"]})
                task = kernel.call("task/start", {"sessionId": session["id"], "input": "调用远程工具"})
                waiting = kernel.wait(task["id"], "waiting_approval")
                assert handler.calls == 0
                print("Proposed tool:", waiting["pendingApproval"]["call"]["name"])
                print("Arguments:", waiting["pendingApproval"]["call"]["arguments"])
                kernel.call("approval/respond", {"taskId": task["id"],
                                                 "approvalId": waiting["pendingApproval"]["id"],
                                                 "decision": "deny" if args.deny else "allow_once"})
                done = kernel.wait(task["id"], "failed" if args.deny else "completed")
                events = kernel.call("events/list", {"taskId": task["id"]})["events"]
                assert handler.calls == (0 if args.deny else 1)
                saved = json.dumps([done, events], ensure_ascii=False)
                assert "fixture-mcp-token" not in saved and "fixture-vendor-key" not in saved
                print("Protocol:", "2025-11-25" if args.legacy else "2026-07-28")
                print("Result:", done["status"], "/ tool calls:", handler.calls)
                print("Output:", done["output"])
                print("MCP methods:", " -> ".join(handler.methods))
                print("Events:", " -> ".join(event["data"]["type"] for event in events))
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    main()
