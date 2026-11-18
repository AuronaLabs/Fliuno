"""Exercise Fliuno against the official A2A Python SDK on loopback."""
import asyncio
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
from importlib.metadata import version

import uvicorn
from starlette.applications import Starlette
from a2a.helpers.proto_helpers import new_task_from_user_message
from a2a.server.agent_execution import AgentExecutor
from a2a.server.request_handlers import DefaultRequestHandler
from a2a.server.routes import create_agent_card_routes, create_jsonrpc_routes
from a2a.server.tasks import InMemoryTaskStore, TaskUpdater
from a2a.types import AgentCapabilities, AgentCard, AgentInterface, AgentSkill, Part

from rpc_client import LocalKernel, build_server


class Reports(AgentExecutor):
    async def execute(self, context, queue):
        if context.current_task is None:
            await queue.enqueue_event(new_task_from_user_message(context.message))
        updater = TaskUpdater(queue, context.task_id, context.context_id)
        prompt = context.message.parts[0].text
        await updater.start_work()
        if prompt == "prepare a report":
            await updater.requires_input(updater.new_agent_message([Part(text="Which quarter?")]))
            return
        await updater.add_artifact(
            [Part(text=f"Report: {prompt}")], name="report.txt", last_chunk=True,
        )
        await updater.complete()

    async def cancel(self, context, queue):
        await TaskUpdater(queue, context.task_id, context.context_id).cancel()


def finished(kernel, task):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        snapshot = kernel.call("task/get", {"taskId": task["id"]})
        if (snapshot.get("agentRun") or {}).get("finished"):
            return snapshot
        time.sleep(0.02)
    raise TimeoutError(f"{task['id']} did not finish")


def check_conversation(executable, root, base_url, streaming):
    config = root / "a2a.json"
    config.write_text(json.dumps({"agents": [{
        "id": "sdk", "endpoint": base_url + "/rpc",
        "agentCardUrl": base_url + "/.well-known/agent-card.json",
        "streaming": streaming, "pollIntervalMs": 25,
    }]}), encoding="utf-8")
    arguments = ["--db", root / "kernel.db", "--a2a-config", config]
    with LocalKernel(executable, arguments, dict(os.environ, FLIUNO_PROVIDER="demo")) as kernel:
        init = kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "official-a2a-sdk"})
        assert init["capabilities"]["remoteTaskContinuation"]
        workspace = kernel.call("workspace/open", {"root": str(root)})
        session = kernel.call("session/create", {"workspaceId": workspace["id"]})
        source = finished(kernel, kernel.call("task/start", {
            "sessionId": session["id"], "input": "prepare a report",
            "backend": {"type": "external_agent", "agentId": "sdk"},
        }))
        assert source["agentRun"]["stopReason"] == "input_required", source
        assert source["agentRun"]["outcomeUnknown"] is False, source
        first_remote = source["agentRun"]["remoteTask"]
        request = {"taskId": source["id"], "input": "quarter two", "clientRequestId": "answer"}
        answer = finished(kernel, kernel.call("task/continueRemote", request))
        assert answer["status"] == "completed", answer
        assert answer["agentRun"]["remoteTask"]["id"] == first_remote["id"], answer
        assert kernel.call("task/continueRemote", request)["id"] == answer["id"]
        next_turn = finished(kernel, kernel.call("task/continueRemote", {
            "taskId": answer["id"], "input": "quarter three", "clientRequestId": "next",
        }))
        assert next_turn["status"] == "completed", next_turn
        next_remote = next_turn["agentRun"]["remoteTask"]
        assert next_remote["id"] != first_remote["id"], next_turn
        assert next_remote["contextId"] == first_remote["contextId"], next_turn
        for turn, prompt in [(answer, "quarter two"), (next_turn, "quarter three")]:
            artifacts = kernel.call("artifacts/list", {"taskId": turn["id"]})
            assert len(artifacts) == 1, artifacts
            assert json.loads(artifacts[0]["content"])["parts"][0]["text"] == f"Report: {prompt}"
        assert kernel.call("task/get", {"taskId": source["id"]}) == source
    print(f"A2A SDK {version('a2a-sdk')} {'SSE' if streaming else 'JSON polling'}: pause, answer, follow-up, artifacts")


def main():
    executable = build_server()
    with socket.socket() as listener, tempfile.TemporaryDirectory(prefix="fliuno-a2a-sdk-") as temp:
        listener.bind(("127.0.0.1", 0))
        base_url = f"http://127.0.0.1:{listener.getsockname()[1]}"
        card = AgentCard(
            name="SDK Reports", description="Independent A2A test peer", version="1.0.0",
            capabilities=AgentCapabilities(streaming=True), default_input_modes=["text/plain"],
            default_output_modes=["text/plain"],
            skills=[AgentSkill(id="reports", name="Reports", description="Produce a report", tags=["test"])],
            supported_interfaces=[AgentInterface(
                protocol_binding="JSONRPC", protocol_version="1.0", url=base_url + "/rpc",
            )],
        )
        handler = DefaultRequestHandler(Reports(), InMemoryTaskStore(), card)
        app = Starlette(routes=[*create_agent_card_routes(agent_card=card),
                                *create_jsonrpc_routes(handler, "/rpc")])
        server = uvicorn.Server(uvicorn.Config(
            app, log_level="warning", timeout_graceful_shutdown=2,
        ))

        def serve():
            # Use a selector loop for loopback sockets on Windows, where
            # aborted requests can leave Proactor connection cleanup pending.
            with asyncio.Runner(loop_factory=asyncio.SelectorEventLoop) as runner:
                runner.run(server.serve(sockets=[listener]))

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        def shutdown():
            server.should_exit = True
            worker.join(timeout=5)
            return not worker.is_alive()

        try:
            deadline = time.monotonic() + 5
            while worker.is_alive() and not server.started and time.monotonic() < deadline:
                time.sleep(0.01)
            if not server.started:
                raise RuntimeError("SDK server did not listen")
            for streaming in [False, True]:
                root = Path(temp) / ("sse" if streaming else "poll")
                root.mkdir()
                check_conversation(executable, root, base_url, streaming)
        except BaseException as error:
            if not shutdown():
                error.add_note("SDK server did not exit")
            raise
        else:
            if not shutdown():
                raise RuntimeError("SDK server did not exit")


if __name__ == "__main__":
    main()
