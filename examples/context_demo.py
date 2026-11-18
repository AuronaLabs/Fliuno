"""Offline executable demo of history delivery, frozen inputs and restart.

The model is a deterministic loopback HTTP fixture. It inspects actual wire
requests; no model credentials, paid requests or host file writes are used.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import threading

from rpc_client import LocalKernel, build_server


class ModelFixture(BaseHTTPRequestHandler):
    received = []
    failures = []

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/responses"
            assert self.headers["Authorization"] == "Bearer fixture-context-key"
            length = int(self.headers.get("Content-Length", "0"))
            assert 0 < length < 1024 * 1024
            request = json.loads(self.rfile.read(length))
            messages = request["input"]
            # Only the initial task has a native tool continuation. Historical
            # calls in later tasks must remain inside the JSON text data block.
            if any(message.get("type") == "function_call_output" for message in messages):
                output = self.text("Report created through an approved tool.")
            else:
                history = None
                if len(messages) == 2:
                    assert all(message.get("role") == "user" for message in messages)
                    history = json.loads(messages[0]["content"].split("\n", 1)[1])
                current = messages[-1]["content"]
                ModelFixture.received.append((current, history))
                if current == "Create a report":
                    assert history is None
                    output = [{"type": "reasoning", "id": "reasoning-1",
                               "encrypted_content": "fixture-private-continuation"},
                              {"type": "function_call", "call_id": "create-report",
                               "name": "artifact_create", "arguments": json.dumps({
                                   "title": "Context report", "content": "记住这份报告。",
                               }, ensure_ascii=False)}]
                elif current == "Unrelated large answer":
                    assert history is None
                    output = self.text("x" * 262144)
                else:
                    assert "fixture-private-continuation" not in json.dumps(history)
                    turns = history["turns"] if history else []
                    output = self.text(json.dumps({
                        "sourceTasks": [turn["taskId"] for turn in turns],
                        "artifactIds": [artifact["id"] for turn in turns
                                        for artifact in turn["artifacts"]],
                    }))
            self.reply(200, {"status": "completed", "output": output})
        except Exception as error:
            ModelFixture.failures.append(repr(error))
            self.reply(500, {"error": "fixture contract failed"})

    @staticmethod
    def text(value):
        return [{"type": "message", "content": [{"type": "output_text", "text": value}]}]

    def reply(self, status, value):
        body = json.dumps(value, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)


def connect(executable, database, environment):
    return LocalKernel(executable, ["--db", database], environment)


def initialize(kernel):
    capabilities = kernel.call("initialize", {
        "protocolVersion": "0.1", "clientName": "context-demo",
    })["capabilities"]
    assert capabilities["sessionContext"]
    assert capabilities["contextTaskSelection"]


def expect_error(kernel, method, params, code):
    try:
        kernel.call(method, params)
    except RuntimeError as error:
        assert error.args[0]["error"]["code"] == code, error
    else:
        raise AssertionError(f"{method} unexpectedly accepted invalid context")


def select_history(kernel, workspace, session, first, follow, artifact):
    """Inspect real model inputs and RPC rejections after a huge newest turn."""
    session_id = session["id"]
    huge = kernel.call("task/start", {
        "sessionId": session_id, "input": "Unrelated large answer",
        "context": {"mode": "isolated"},
    })
    kernel.wait(huge["id"], "completed")
    assert kernel.call("session/context", {"sessionId": session_id})["turns"] == []
    other = kernel.call("session/create", {"workspaceId": workspace["id"]})
    foreign = kernel.call("task/start", {
        "sessionId": other["id"], "input": "Other session",
    })
    kernel.wait(foreign["id"], "completed")
    pending = kernel.call("task/start", {
        "sessionId": session_id, "input": "Create a report",
        "context": {"mode": "isolated"},
    })
    kernel.wait(pending["id"], "waiting_approval")
    tasks_before = kernel.call("task/list", {"sessionId": session_id})
    watermark = kernel.call("session/context", {
        "sessionId": session_id, "options": {"mode": "isolated"},
    })["throughSequence"]
    received_before = len(ModelFixture.received)
    for options in [
        {"taskIds": []}, {"taskIds": ["missing"]},
        {"taskIds": [foreign["id"]]}, {"taskIds": [pending["id"]]},
        {"taskIds": [first["id"], first["id"]]},
        {"taskIds": [first["id"]], "mode": "isolated"},
        {"taskIds": [first["id"], follow["id"]], "maxTurns": 1},
        {"taskIds": [huge["id"]], "maxBytes": 1024},
    ]:
        expect_error(kernel, "session/context", {
            "sessionId": session_id, "options": options,
        }, -32602)
        expect_error(kernel, "task/start", {
            "sessionId": session_id, "input": "Selected historical report",
            "clientRequestId": "explicit-selection", "context": options,
        }, -32602)
    assert kernel.call("task/list", {"sessionId": session_id}) == tasks_before
    assert kernel.call("session/context", {
        "sessionId": session_id, "options": {"mode": "isolated"},
    })["throughSequence"] == watermark
    assert len(ModelFixture.received) == received_before
    options = {"taskIds": [follow["id"], first["id"]]}
    preview = kernel.call("session/context", {"sessionId": session_id, "options": options})
    assert [turn["taskId"] for turn in preview["turns"]] == [first["id"], follow["id"]]
    assert preview["options"]["taskIds"] == sorted(options["taskIds"])
    assert preview["omittedTurns"] == 3
    selected = kernel.call("task/start", {
        "sessionId": session_id, "input": "Selected historical report",
        "clientRequestId": "explicit-selection", "context": options,
    })
    assert selected["context"] == preview
    done = kernel.wait(selected["id"], "completed")
    assert json.loads(done["output"]) == {
        "sourceTasks": [first["id"], follow["id"]], "artifactIds": [artifact["id"]],
    }
    assert ModelFixture.received[-1] == ("Selected historical report", preview)
    expect_error(kernel, "task/start", {
        "sessionId": session_id, "input": "Selected historical report",
        "clientRequestId": "explicit-selection", "context": {"taskIds": [first["id"]]},
    }, -32009)
    assert kernel.call("artifacts/list", {"taskId": pending["id"]}) == []
    kernel.call("task/cancel", {"taskId": pending["id"]})
    print("Explicit selection skipped huge history; invalid selections wrote no tasks or events.")
    return selected


def main():
    executable = build_server()
    server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
    serving = threading.Thread(target=server.serve_forever, daemon=True)
    serving.start()
    environment = dict(os.environ, FLIUNO_PROVIDER="openai", FLIUNO_MODEL="fixture-model",
                       OPENAI_API_KEY="fixture-context-key",
                       OPENAI_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1/")
    try:
        with tempfile.TemporaryDirectory(prefix="fliuno-context-") as temporary:
            database = Path(temporary) / "kernel.db"
            with connect(executable, database, environment) as kernel:
                initialize(kernel)
                workspace = kernel.call("workspace/open", {"root": temporary})
                session = kernel.call("session/create", {"workspaceId": workspace["id"]})
                first = kernel.call("task/start", {
                    "sessionId": session["id"], "input": "Create a report",
                })
                waiting = kernel.wait(first["id"], "waiting_approval")
                assert kernel.call("artifacts/list", {"taskId": first["id"]}) == []
                kernel.call("approval/respond", {
                    "taskId": first["id"], "approvalId": waiting["pendingApproval"]["id"],
                    "decision": "allow_once",
                })
                kernel.wait(first["id"], "completed")
                artifact = kernel.call("artifacts/list", {"taskId": first["id"]})[0]
                preview = kernel.call("session/context", {"sessionId": session["id"]})
                follow = kernel.call("task/start", {
                    "sessionId": session["id"], "input": "Continue the report",
                    "clientRequestId": "follow-up",
                })
                assert follow["context"] == preview
                done = kernel.wait(follow["id"], "completed")
                assert json.loads(done["output"]) == {
                    "sourceTasks": [first["id"]], "artifactIds": [artifact["id"]],
                }
                print("Follow-up received the completed tool trace and artifact identity.")

            with connect(executable, database, environment) as kernel:
                initialize(kernel)
                retry = kernel.call("task/start", {
                    "sessionId": session["id"], "input": "Continue the report",
                    "clientRequestId": "follow-up",
                })
                assert retry["id"] == follow["id"] and retry["context"] == preview
                print("Restart preserved the original snapshot and idempotent task identity.")
                next_task = kernel.call("task/start", {
                    "sessionId": session["id"], "input": "Continue after restart",
                })
                done = kernel.wait(next_task["id"], "completed")
                assert json.loads(done["output"])["sourceTasks"] == [first["id"], follow["id"]]
                isolated = kernel.call("task/start", {
                    "sessionId": session["id"], "input": "Independent task",
                    "context": {"mode": "isolated"},
                })
                done = kernel.wait(isolated["id"], "completed")
                assert json.loads(done["output"]) == {"sourceTasks": [], "artifactIds": []}
                print("New tasks recovered history; isolated mode sent no history block.")
                selected = select_history(kernel, workspace, session, first, follow, artifact)
            with connect(executable, database, environment) as kernel:
                initialize(kernel)
                retry = kernel.call("task/start", {
                    "sessionId": session["id"], "input": "Selected historical report",
                    "clientRequestId": "explicit-selection",
                    "context": {"taskIds": [first["id"], follow["id"]]},
                })
                assert retry["id"] == selected["id"]
                assert retry["context"] == selected["context"]
                print("Restart and reordered task IDs preserved the selected snapshot without replay.")
            assert len(ModelFixture.received) == 8
            assert not ModelFixture.failures, ModelFixture.failures
    finally:
        server.shutdown()
        serving.join(timeout=5)
        server.server_close()


if __name__ == "__main__":
    main()
