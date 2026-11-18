"""Offline progressive Skills workflow, with real RPC, approvals and restart.

--deny rejects a resource read. --switch-source restarts against a different
catalog while retaining the same skill/tool names and proves old consent fails.
"""
import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import threading

from rpc_client import LocalKernel, build_server

INSTRUCTIONS = "---\nname: example\ndescription: Progressive fixture\nallowed-tools: shell\n---\nRead references/guide.md when needed. Scripts require separate host registration.\n"
GUIDE = "独立参考资料：RESOURCE_ONLY_AFTER_APPROVAL\n"
ASSET = bytes([0, 255, 13, 10, 128]) + b"OPAQUE_SKILL_ASSET"


class ModelFixture(BaseHTTPRequestHandler):
    failures = []
    calls = 0
    resource = GUIDE
    script = ""

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.path == "/v1/responses"
            assert self.headers["Authorization"] == "Bearer fixture-skills-key"
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            ModelFixture.calls += 1
            results = [json.loads(item["output"]) for item in request["input"]
                       if item.get("type") == "function_call_output"]
            steps = [
                ("skills_list", {}),
                ("skills_read", {"name": "example"}),
                ("skills_list_resources", {"name": "example", "path": "references"}),
                ("skills_read_resource", {"name": "example", "path": "references/guide.md"}),
                ("skills_read_resource", {"name": "example", "path": "scripts/never-run.py"}),
                ("skills_read_resource", {"name": "example", "path": "assets/template.bin", "encoding": "base64"}),
            ]
            available = {tool["name"] for tool in request["tools"]}
            assert all(name in available for name, _ in steps)
            if results:
                assert results[0][0]["name"] == "example"
                assert "Read references" not in json.dumps(results[0])
            if len(results) >= 2:
                assert results[1]["instructions"] == INSTRUCTIONS
                assert results[1]["grantsPermissions"] is False
            if len(results) >= 3:
                assert results[2]["entries"] == [
                    {"name": "guide.md", "kind": "file", "bytes": len(self.resource.encode())},
                ]
            if len(results) >= 4:
                assert results[3]["content"] == self.resource
                assert results[3]["sha256"] == hashlib.sha256(self.resource.encode()).hexdigest()
                assert results[3]["grantsPermissions"] is False
            if len(results) >= 5:
                assert results[4]["content"] == self.script
                assert results[4]["grantsPermissions"] is False
            if len(results) >= 6:
                assert results[5]["encoding"] == "base64"
                assert base64.b64decode(results[5]["data"], validate=True) == ASSET
                assert results[5]["bytes"] == len(ASSET)
                assert results[5]["sha256"] == hashlib.sha256(ASSET).hexdigest()
                assert results[5]["grantsPermissions"] is False
            if len(results) < len(steps):
                name, arguments = steps[len(results)]
                output = [{"type": "function_call", "call_id": f"step-{len(results)}",
                           "name": name, "arguments": json.dumps(arguments)}]
            else:
                output = [{"type": "message", "content": [
                    {"type": "output_text", "text": "Skill resource workflow complete"},
                ]}]
            body = json.dumps({"status": "completed", "output": output}).encode()
            self.send_response(200)
        except Exception as error:
            ModelFixture.failures.append(repr(error))
            body = b'{"error":"fixture contract failed"}'
            self.send_response(500)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)


def catalog(path, script, guide):
    skill = path / "example"
    (skill / "references").mkdir(parents=True)
    (skill / "scripts").mkdir()
    (skill / "assets").mkdir()
    (skill / "SKILL.md").write_bytes(INSTRUCTIONS.encode())
    (skill / "references" / "guide.md").write_bytes(guide.encode())
    (skill / "scripts" / "never-run.py").write_bytes(script.encode())
    (skill / "assets" / "template.bin").write_bytes(ASSET)


def initialize(kernel):
    assert kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "skills-demo"})[
        "capabilities"]["registrationAuthority"]


def workflow(kernel, session, mode="allow"):
    task = kernel.call("task/start", {
        "sessionId": session, "input": "Inspect this skill progressively",
        "context": {"mode": "isolated"},
    })
    names = ["skills_list", "skills_read", "skills_list_resources",
             "skills_read_resource", "skills_read_resource", "skills_read_resource"]
    for index, name in enumerate(names):
        waiting = kernel.wait(task["id"], "waiting_approval")
        assert waiting["pendingApproval"]["call"]["name"] == name
        if index == 3 and mode == "switch":
            return waiting
        decision = "deny" if index == 3 and mode == "deny" else "allow_once"
        kernel.call("approval/respond", {
            "taskId": task["id"], "approvalId": waiting["pendingApproval"]["id"],
            "decision": decision,
        })
        if decision == "deny":
            failed = kernel.wait(task["id"], "failed")
            events = kernel.call("events/list", {"taskId": task["id"]})
            assert "RESOURCE_ONLY_AFTER_APPROVAL" not in json.dumps(events)
            assert len([event for event in events["events"]
                        if event["data"]["type"] == "tool_completed"]) == 3
            return failed
    return kernel.wait(task["id"], "completed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    choices = parser.add_mutually_exclusive_group()
    choices.add_argument("--deny", action="store_true")
    choices.add_argument("--switch-source", action="store_true")
    args = parser.parse_args()
    executable = build_server()
    server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
    serving = threading.Thread(target=server.serve_forever, daemon=True)
    serving.start()
    environment = dict(os.environ, FLIUNO_PROVIDER="openai", FLIUNO_MODEL="fixture-model",
                       OPENAI_API_KEY="fixture-skills-key",
                       OPENAI_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1/")
    try:
        with tempfile.TemporaryDirectory(prefix="fliuno-skills-demo-") as temporary:
            root = Path(temporary)
            marker = root / "SCRIPT_MUST_NOT_EXECUTE"
            ModelFixture.script = f"from pathlib import Path\nPath({str(marker)!r}).write_text('executed')\n"
            source = root / "catalog-one"
            catalog(source, ModelFixture.script, GUIDE)
            database = root / "kernel.db"

            def connect(selected):
                return LocalKernel(executable, ["--db", database, "--skills-dir", selected], environment)

            with connect(source) as kernel:
                initialize(kernel)
                workspace = kernel.call("workspace/open", {"root": temporary})
                session = kernel.call("session/create", {"workspaceId": workspace["id"]})["id"]
                result = workflow(kernel, session,
                                  "switch" if args.switch_source else "deny" if args.deny else "allow")
            if args.switch_source:
                replacement = root / "catalog-two"
                ModelFixture.resource = GUIDE + "Replacement catalog\n"
                catalog(replacement, ModelFixture.script, ModelFixture.resource)
                before = ModelFixture.calls
                with connect(replacement) as kernel:
                    initialize(kernel)
                    kernel.call("approval/respond", {
                        "taskId": result["id"], "approvalId": result["pendingApproval"]["id"],
                        "decision": "allow_once",
                    })
                    failed = kernel.wait(result["id"], "failed")
                    assert "registration changed" in failed["output"]
                    assert ModelFixture.calls == before
                    print("Changed catalog rejected old resource consent without another model request.")
                    result = workflow(kernel, session)
            assert not marker.exists(), "reading a script must never execute it"
            assert not ModelFixture.failures, ModelFixture.failures
            print("Skills:", "metadata -> instructions -> listing -> reference -> script text -> opaque asset")
            print("Result:", result["status"], "/ script executed:", marker.exists())
    finally:
        server.shutdown()
        server.server_close()
        serving.join(timeout=5)


if __name__ == "__main__":
    main()
