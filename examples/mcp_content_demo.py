"""Offline MCP resources/prompts through real app-server, consent and restart.

--deny stops before resource content; --switch-source changes the registered
endpoint during a pending read and checks that old consent cannot move with it.
Both model and MCP peers are deterministic loopback fixtures.
"""
import argparse
import base64
from hashlib import sha256
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import threading

from rpc_client import LocalKernel, build_server

STEPS = [
    ("resources_list", {}),
    ("resources_list", {"cursor": "second"}),
    ("resources_templates", {}),
    ("resources_read", {"uri": "fixture:report/工作"}),
    ("prompts_list", {}),
    ("prompts_get", {"name": "draft", "arguments": {"topic": "工作成果"}}),
    ("resources_import", {"uri": "fixture:report/附件", "title": "Quarterly report"}),
]
PROMPT = "UNTRUSTED_REMOTE_PROMPT_STAYS_IN_TOOL_RESULT"
ATTACHMENT_TEXT = "IMPORTED_BODY_STAYS_OUT_OF_MODEL_CONTEXT"
ATTACHMENT_BYTES = b"\x00\x01\x02\xff\xfe"


class Peer(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    failures = []
    methods = []
    model_calls = 0
    source = "/mcp"

    def log_message(self, *_args):
        pass

    def reply(self, status, value):
        body = json.dumps(value, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def do_GET(self):
        self.reply(405, None)

    def do_DELETE(self):
        self.reply(200, None)

    def model(self, request):
        assert self.headers["Authorization"] == "Bearer fixture-content-model-key"
        Peer.model_calls += 1
        assert PROMPT not in request.get("instructions", "")
        for item in request["input"]:
            if item.get("type") != "function_call_output":
                assert PROMPT not in json.dumps(item)
        results = [json.loads(item["output"]) for item in request["input"]
                   if item.get("type") == "function_call_output"]
        for result in results:
            assert result["grantsPermissions"] is False
            assert result["serverId"] == "remote"
        if len(results) >= 1:
            assert results[0]["result"]["nextCursor"] == "second"
        if len(results) >= 2:
            assert results[1]["result"]["resources"][0]["name"] == "second"
        if len(results) >= 3:
            assert "uriTemplate" in results[2]["result"]["resourceTemplates"][0]
        if len(results) >= 4:
            contents = results[3]["result"]["contents"]
            assert contents[0]["text"] == f"CONTENT_ONLY_AFTER_CONSENT:{self.source}"
            assert contents[1]["blob"] == "AAH/"
        if len(results) >= 5:
            assert results[4]["result"]["prompts"][0]["name"] == "draft"
        if len(results) >= 6:
            messages = results[5]["result"]["messages"]
            assert messages[0]["role"] == "user"
            assert messages[0]["content"]["text"] == PROMPT
            assert messages[1]["content"]["text"] == "工作成果"
        if len(results) >= 7:
            imported = results[6]["result"]
            assert imported["uri"] == "fixture:report/附件"
            contents = imported["contents"]
            assert len(contents) == 2 and all(item["artifactId"] for item in contents)
            assert contents[0]["binary"] is False and contents[1]["binary"] is True
            assert ATTACHMENT_TEXT not in json.dumps(request)
            assert base64.b64encode(ATTACHMENT_BYTES).decode() not in json.dumps(request)
        if len(results) < len(STEPS):
            label, arguments = STEPS[len(results)]
            name = "mcp_remote_" + label
            assert name in {tool["name"] for tool in request["tools"]}
            output = [{"type": "function_call", "call_id": f"step-{len(results)}",
                       "name": name, "arguments": json.dumps(arguments, ensure_ascii=False)}]
        else:
            output = [{"type": "message", "content": [
                {"type": "output_text", "text": "MCP content workflow complete"},
            ]}]
        return {"status": "completed", "output": output}

    def mcp(self, request):
        assert self.path in {"/mcp", "/mcp-replacement"}
        assert self.headers["Authorization"] == "Bearer fixture-content-mcp-key"
        method = request["method"]
        assert self.headers["Mcp-Method"] == method
        Peer.methods.append((self.path, method))
        params = request.get("params", {})
        common = {"resultType": "complete", "ttlMs": 60000, "cacheScope": "private"}
        if method == "server/discover":
            result = dict(common, supportedVersions=["2026-07-28"],
                          capabilities={"resources": {}, "prompts": {}})
        elif method == "resources/list":
            second = params.get("cursor") == "second"
            result = dict(common, resources=[{"name": "second" if second else "first",
                                              "uri": "fixture:second" if second else "fixture:first"}])
            if not second:
                result["nextCursor"] = "second"
        elif method == "resources/templates/list":
            result = dict(common, resourceTemplates=[{"name": "report", "uriTemplate": "fixture:report/{id}"}])
        elif method == "resources/read":
            if params["uri"] == "fixture:report/附件":
                assert set(params) - {"_meta"} == {"uri"}
                result = dict(common, contents=[
                    {"uri": params["uri"], "text": ATTACHMENT_TEXT},
                    {"uri": "fixture:binary", "mimeType": "invalid/type?",
                     "blob": base64.b64encode(ATTACHMENT_BYTES).decode()},
                ])
            else:
                assert params["uri"] == "fixture:report/工作"
                result = dict(common, contents=[
                    {"uri": params["uri"], "mimeType": "text/plain", "text": f"CONTENT_ONLY_AFTER_CONSENT:{self.path}"},
                    {"uri": "fixture:binary", "mimeType": "application/octet-stream", "blob": "AAH/"},
                ])
        elif method == "prompts/list":
            result = dict(common, prompts=[{"name": "draft", "arguments": [{"name": "topic", "required": True}]}])
        elif method == "prompts/get":
            assert params["name"] == "draft"
            result = {"resultType": "complete", "messages": [
                {"role": "user", "content": {"type": "text", "text": PROMPT}},
                {"role": "assistant", "content": {"type": "text", "text": params["arguments"]["topic"]}},
            ]}
        else:
            raise AssertionError(f"unexpected MCP method {method}")
        return {"jsonrpc": "2.0", "id": request["id"], "result": result}

    def do_POST(self):
        try:
            length = int(self.headers["Content-Length"])
            assert 0 < length <= 1024 * 1024
            request = json.loads(self.rfile.read(length))
            result = self.model(request) if self.path == "/v1/responses" else self.mcp(request)
            self.reply(200, result)
        except Exception as error:
            Peer.failures.append(repr(error))
            self.reply(500, {"error": "fixture contract failed"})


def count(method):
    return sum(1 for _, observed in Peer.methods if observed == method)


def workflow(kernel, session, mode="allow"):
    task = kernel.call("task/start", {"sessionId": session, "input": "Inspect MCP resources and prompt data",
                                      "context": {"mode": "isolated"}})
    for index, (label, _) in enumerate(STEPS):
        waiting = kernel.wait(task["id"], "waiting_approval")
        assert waiting["pendingApproval"]["call"]["name"] == "mcp_remote_" + label
        assert count("resources/read") == (0 if index <= 3 else 1)
        assert count("prompts/get") == int(index == len(STEPS) - 1)
        if index == 3 and mode == "switch":
            return waiting
        kernel.call("approval/respond", {"taskId": task["id"], "approvalId": waiting["pendingApproval"]["id"],
                                          "decision": "deny" if index == 3 and mode == "deny" else "allow_once"})
        if index == 3 and mode == "deny":
            failed = kernel.wait(task["id"], "failed")
            assert count("resources/read") == 0
            return failed
    return kernel.wait(task["id"], "completed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--deny", action="store_true")
    modes.add_argument("--switch-source", action="store_true")
    args = parser.parse_args()
    executable = build_server()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Peer)
    serving = threading.Thread(target=server.serve_forever, daemon=True)
    serving.start()
    base = f"http://127.0.0.1:{server.server_port}"
    environment = dict(os.environ, FLIUNO_PROVIDER="openai", FLIUNO_MODEL="fixture-model",
                       OPENAI_API_KEY="fixture-content-model-key", OPENAI_BASE_URL=base + "/v1/",
                       FLIUNO_CONTENT_MCP_KEY="fixture-content-mcp-key")
    try:
        with tempfile.TemporaryDirectory(prefix="fliuno-mcp-content-") as temporary:
            root = Path(temporary)
            config = root / "mcp.json"

            def configure(path):
                config.write_text(json.dumps({"servers": [{"id": "remote", "transport": "streamable_http",
                    "url": base + path, "bearerTokenEnv": "FLIUNO_CONTENT_MCP_KEY", "timeoutMs": 5000}]}), encoding="utf-8")

            def connect():
                return LocalKernel(executable, ["--db", root / "kernel.db", "--mcp-config", config], environment)

            def initialize(kernel):
                kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "mcp-content-demo"})

            configure("/mcp")
            with connect() as kernel:
                initialize(kernel)
                workspace = kernel.call("workspace/open", {"root": temporary})
                session = kernel.call("session/create", {"workspaceId": workspace["id"]})["id"]
                result = workflow(kernel, session, "switch" if args.switch_source else "deny" if args.deny else "allow")
            if args.switch_source:
                configure("/mcp-replacement")
                Peer.source = "/mcp-replacement"
                model_calls = Peer.model_calls
                with connect() as kernel:
                    initialize(kernel)
                    kernel.call("approval/respond", {"taskId": result["id"], "approvalId": result["pendingApproval"]["id"],
                                                      "decision": "allow_once"})
                    failed = kernel.wait(result["id"], "failed")
                    assert "registration changed" in failed["output"]
                    assert count("resources/read") == 0 and Peer.model_calls == model_calls
                    result = workflow(kernel, session)
            with connect() as kernel:
                initialize(kernel)
                snapshot = kernel.call("task/get", {"taskId": result["id"]})
                assert snapshot == result
                events = kernel.call("events/list", {"taskId": result["id"]})["events"]
                saved = json.dumps([snapshot, events])
                assert "fixture-content-mcp-key" not in saved
                assert "fixture-content-model-key" not in saved
                if args.deny:
                    assert "CONTENT_ONLY_AFTER_CONSENT" not in saved and PROMPT not in saved
                items = kernel.call("artifacts/listSummaries", {"taskId": result["id"]})["items"]
                if args.deny:
                    assert not items
                else:
                    assert len(items) == 2 and ATTACHMENT_TEXT not in saved
                    assert items[0]["title"] == "Quarterly report (1/2)"
                    assert items[0]["mediaType"] == "text/plain"
                    assert items[1]["binary"] and items[1]["mediaType"] == "application/octet-stream"
                    binary = {"taskId": result["id"], "artifactId": items[1]["id"]}
                    part = kernel.call("artifacts/read", dict(binary, after=1, limit=3))
                    assert base64.b64decode(part["dataBase64"]) == ATTACHMENT_BYTES[1:4]
                    assert kernel.call("artifacts/digest", binary)["sha256"] == sha256(ATTACHMENT_BYTES).hexdigest()
            assert count("tools/list") == 0
            assert count("resources/read") == (0 if args.deny else 2)
            assert count("prompts/get") == (0 if args.deny else 1)
            assert not Peer.failures, Peer.failures
            print("MCP content: pages -> templates -> resource -> prompts -> artifact import")
            print("Result:", result["status"], "/ reads:", count("resources/read"), "/ prompt gets:", count("prompts/get"))
            print("Restart preserved the task; prompt roles remained function-call output data.")
    finally:
        server.shutdown()
        server.server_close()
        serving.join(timeout=5)


if __name__ == "__main__":
    main()
