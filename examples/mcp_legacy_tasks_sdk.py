"""Run legacy MCP Tasks with the official Python SDK (mcp==1.25.0)."""
import argparse
import asyncio
from contextlib import asynccontextmanager, contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib.metadata import version
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import threading
import time

from mcp import types
from mcp.server.lowlevel import Server
from mcp.server.stdio import stdio_server
from mcp.server.streamable_http_manager import StreamableHTTPSessionManager
from starlette.applications import Starlette
from starlette.routing import Mount
import uvicorn

from rpc_client import LocalKernel, build_server


class LegacyTaskServer(Server):
    def get_capabilities(self, notification_options, experimental_capabilities):
        capabilities = super().get_capabilities(notification_options, experimental_capabilities)
        # SDK 1.25.0 advertises an empty tools object. The 2025-11-25 spec
        # requires the call sub-capability before clients may augment calls.
        capabilities.tasks.requests.tools.call = types.TasksCallCapability()
        return capabilities


def sdk_server():
    server = LegacyTaskServer("legacy-sdk-tasks")
    server.experimental.enable_tasks()

    @server.list_tools()
    async def list_tools():
        return [types.Tool(
            name="report", inputSchema={"type": "object", "additionalProperties": False,
                                        "properties": {"withForm": {"type": "boolean"}}},
            outputSchema={"type": "object", "properties": {"rows": {"type": "integer"}},
                          "required": ["rows"]},
            execution=types.ToolExecution(taskSupport="required"),
        )]

    @server.call_tool()
    async def call_tool(name, arguments):
        assert name == "report"

        async def report(task):
            rows = 42
            if arguments.get("withForm"):
                form = await task.elicit("How many rows?", {
                    "type": "object", "properties": {"rows": {"type": "integer"}}, "required": ["rows"],
                })
                assert form.action == "accept"
                rows = form.content["rows"]
            return types.CallToolResult(content=[], structuredContent={"rows": rows})

        return await server.request_context.experimental.run_task(report)

    return server


async def serve_sdk():
    server = sdk_server()
    async with stdio_server() as (reader, writer):
        await server.run(reader, writer, server.create_initialization_options())


@contextmanager
def http_sdk(*, json_response):
    manager = StreamableHTTPSessionManager(sdk_server(), json_response=json_response)

    @asynccontextmanager
    async def lifespan(_app):
        async with manager.run():
            yield

    app = Starlette(routes=[Mount("/", app=manager.handle_request)], lifespan=lifespan)
    server = uvicorn.Server(uvicorn.Config(app, log_level="warning", timeout_graceful_shutdown=2))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        url = f"http://127.0.0.1:{listener.getsockname()[1]}/mcp"

        def serve():
            with asyncio.Runner(loop_factory=asyncio.SelectorEventLoop) as runner:
                runner.run(server.serve(sockets=[listener]))

        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        try:
            deadline = time.monotonic() + 5
            while thread.is_alive() and not server.started and time.monotonic() < deadline:
                time.sleep(0.01)
            if not server.started:
                raise RuntimeError("SDK server did not listen")
            yield url
        finally:
            server.should_exit = True
            thread.join(timeout=5)
            if thread.is_alive():
                raise RuntimeError("SDK server did not exit")


class ModelFixture(BaseHTTPRequestHandler):
    with_form = False

    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        tools = {tool["name"] for tool in request["tools"]}
        results = [json.loads(item["output"]) for item in request["input"]
                   if item.get("type") == "function_call_output"]
        step = len(results)
        if step == 0:
            reports = sorted(name for name in tools if name.startswith("mcp_sdk_report_"))
            assert len(reports) == 1, sorted(tools)
            name = reports[0]
            arguments = {"withForm": self.with_form}
        elif step in (1, 2):
            assert results[0]["protocolVersion"] == "2025-11-25"
            task_id = results[0]["task"]["taskId"]
            arguments = {"taskId": task_id}
            if step == 1:
                name = "mcp_sdk_tasks_wait"
                arguments["waitMs"] = 1000
            else:
                name = "mcp_sdk_tasks_result"
                assert results[1]["result"]["snapshot"]["status"] == (
                    "input_required" if self.with_form else "completed"
                )
        else:
            assert step == 3
            assert results[2]["result"]["structuredContent"] == {"rows": 42}
            related = results[2]["result"]["_meta"]["io.modelcontextprotocol/related-task"]
            assert related["taskId"] == results[0]["task"]["taskId"]
        if step < 3:
            assert name in tools
            output = [{"type": "function_call", "call_id": f"legacy-task-{step}",
                       "name": name, "arguments": json.dumps(arguments)}]
        else:
            output = [{"type": "message", "content": [
                {"type": "output_text", "text": "SDK task returned 42 rows"},
            ]}]
        reply = json.dumps({"status": "completed", "output": output}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


def verify(executable, transport, url=None, *, with_form=False, restart=False):
    handler = type("Model", (ModelFixture,), {"with_form": with_form})
    with ThreadingHTTPServer(("127.0.0.1", 0), handler) as model:
        thread = threading.Thread(target=model.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="fliuno-mcp-legacy-sdk-") as temporary:
                root = Path(temporary)
                config = root / "mcp.json"
                server = {"id": "sdk", "lifecycle": "legacy", "timeoutMs": 10000}
                if url is None:
                    server.update(command=sys.executable,
                        args=[str(Path(__file__).resolve()), "--serve-sdk"], cwd=temporary,
                        isolation="trusted_host")
                else:
                    server.update(transport="streamable_http", url=url)
                config.write_text(json.dumps({"servers": [server]}), encoding="utf-8")
                env = dict(os.environ, FLIUNO_PROVIDER="openai", FLIUNO_MODEL="fixture-model",
                           OPENAI_API_KEY="fixture-mcp-sdk-key",
                           OPENAI_BASE_URL=f"http://127.0.0.1:{model.server_port}/v1/")
                command = ["--db", root / "kernel.db", "--mcp-config", config]
                with LocalKernel(executable, command, env) as kernel:
                    kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "legacy-sdk"})
                    workspace = kernel.call("workspace/open", {"root": temporary})
                    session = kernel.call("session/create", {"workspaceId": workspace["id"]})
                    task = kernel.call("task/start", {"sessionId": session["id"], "input": "Run SDK report"})
                    for prefix in ("mcp_sdk_report_", "mcp_sdk_tasks_wait", "mcp_sdk_tasks_result"):
                        waiting = kernel.wait(task["id"], "waiting_approval")
                        approval = waiting["pendingApproval"]
                        assert approval["call"]["name"].startswith(prefix), approval["call"]["name"]
                        kernel.call("approval/respond", {"taskId": task["id"],
                            "approvalId": approval["id"], "decision": "allow_once"})
                    if with_form:
                        waiting = kernel.wait(task["id"], "waiting_input")
                        pending = waiting["pendingToolInput"]
                        request_id, = pending["requests"]
                        request = {"taskId": task["id"], "callId": pending["callId"],
                                   "requestId": "answer-1", "responses": {
                                       request_id: {"action": "accept", "content": {"rows": 42}},
                                   }}
                        rejected = dict(request, responses={request_id: {
                            "action": "accept", "content": {"rows": "invalid"},
                        }})
                        try:
                            kernel.call("task/respondToolInput", rejected)
                        except RuntimeError as error:
                            assert "does not match its requested schema" in str(error), error
                        else:
                            raise AssertionError("invalid form answer was accepted")
                        if not restart:
                            kernel.call("task/respondToolInput", request)
                            kernel.call("task/respondToolInput", request)
                    if not restart:
                        result = kernel.wait(task["id"], "completed")
                        assert result["output"] == "SDK task returned 42 rows"
                if restart:
                    with LocalKernel(executable, command, env) as kernel:
                        kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "legacy-sdk"})
                        recovered = kernel.call("task/get", {"taskId": task["id"]})
                        assert recovered["pendingToolInput"] == pending
                        kernel.call("task/respondToolInput", request)
                        result = kernel.wait(task["id"], "interrupted")
                        assert "no longer has its original live request" in result["output"]
        finally:
            model.shutdown()
            thread.join(timeout=5)
    workflow = ("confirmed the old form was not replayed after restart" if restart else
                "create, wait, form, and result" if with_form else "create, wait, and result")
    print(f"MCP Python SDK {version('mcp')}: {workflow} over {transport}.")


if __name__ == "__main__":
    if version("mcp") != "1.25.0":
        raise SystemExit("This check requires mcp==1.25.0 (2025-11-25).")
    if sys.argv[1:] == ["--serve-sdk"]:
        asyncio.run(serve_sdk())
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--transport", choices=("all", "stdio", "http-json", "http-sse"), default="all")
        workflows = parser.add_mutually_exclusive_group()
        workflows.add_argument("--form", action="store_true", help="Include input (stdio and HTTP SSE only)")
        workflows.add_argument("--restart-form", action="store_true", help="Restart while waiting for input (stdio only)")
        args = parser.parse_args()
        if args.form and args.transport == "http-json":
            parser.error("mcp==1.25.0 does not deliver legacy task forms in HTTP JSON mode")
        if args.restart_form and args.transport not in ("all", "stdio"):
            parser.error("--restart-form requires stdio")
        executable = build_server()
        if args.transport in ("all", "stdio"):
            verify(executable, "stdio", with_form=args.form or args.restart_form, restart=args.restart_form)
        for mode in ("http-json", "http-sse"):
            if not args.restart_form and args.transport in ("all", mode) and (not args.form or mode == "http-sse"):
                with http_sdk(json_response=mode == "http-json") as url:
                    verify(executable, mode, url, with_form=args.form)
