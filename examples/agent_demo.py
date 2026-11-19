"""Offline Windows demo: real isolated ACP agent -> durable consent -> completion.

The explicit test fixture is the only peer. No model calls; workspace writes
affect only the demo's temporary directory.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time

from rpc_client import build_binaries

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deny", action="store_true", help="Deny the fixture permission")
    parser.add_argument("--workspace", action="store_true", help="Review writes and reads through the workspace proxy")
    args = parser.parse_args()
    if os.name != "nt":
        raise SystemExit("This demo requires the Windows AppContainer backend.")
    binaries = build_binaries(fixtures=True)
    with tempfile.TemporaryDirectory(prefix="fliuno-agent-demo-") as directory:
        directory = Path(directory)
        config = directory / "agents.json"
        config.write_text(json.dumps({"agents": [{
            "id": "fixture", "command": str(binaries / "fliuno-app-acp-fixture.exe"),
            "isolation": "native_scratch", "permissionTimeoutMs": 10_000,
            "workspaceAccess": args.workspace,
        }]}), encoding="utf-8")
        child = subprocess.Popen(
            [str(binaries / "fliuno-app-server.exe"), "--db", str(directory / "kernel.db"),
             "--acp-config", str(config)], cwd=ROOT, env=dict(os.environ, FLIUNO_PROVIDER="demo"),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8",
        )
        replies = queue.Queue()

        def read_output():
            try:
                for line in child.stdout:
                    replies.put(json.loads(line))
            finally:
                replies.put(None)

        reader = threading.Thread(target=read_output, daemon=True)
        reader.start()
        request_id = 0

        def call(method, params):
            nonlocal request_id
            request_id += 1
            child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id,
                                         "method": method, "params": params}) + "\n")
            child.stdin.flush()
            deadline = time.monotonic() + 10
            while True:
                response = replies.get(timeout=max(0, deadline - time.monotonic()))
                if response is None:
                    raise RuntimeError(f"server exited with {child.poll()}")
                if "method" in response and "id" not in response:
                    continue
                if response.get("id") != request_id or "error" in response:
                    raise RuntimeError(response)
                return response["result"]

        def wait_for_first_approval(task_id):
            # AppContainer staging precedes two ACP requests, each with a
            # 30-second default timeout. Polling must allow those phases to finish.
            deadline = time.monotonic() + 75
            while True:
                task = call("task/get", {"taskId": task_id})
                if task["status"] == "waiting_approval":
                    return task
                if task["status"] in {"failed", "cancelled", "interrupted", "completed"}:
                    raise RuntimeError(task)
                if time.monotonic() >= deadline:
                    run = task.get("agentRun") or {}
                    phase = "prompt" if run.get("sessionId") else "agent setup"
                    raise TimeoutError(f"task {task_id} awaiting approval during {phase}: {task['status']}")
                time.sleep(0.025)

        try:
            init = call("initialize", {"protocolVersion": "0.1", "clientName": "agent-demo"})
            assert init["capabilities"]["externalAgents"]
            workspace = call("workspace/open", {"root": str(directory)})
            session = call("session/create", {"workspaceId": workspace["id"]})
            submission = {"sessionId": session["id"], "input": "host_roundtrip" if args.workspace else "permission",
                          "clientRequestId": "offline-agent-once",
                          "backend": {"type": "external_agent", "agentId": "fixture"}}
            task = call("task/start", submission)
            waiting = wait_for_first_approval(task["id"])
            decision = "deny" if args.deny else "allow_once"
            if args.workspace:
                assert not (directory / "via-agent.txt").exists()
                print("Review diff:", waiting["agentRun"]["pendingApprovals"][0]["request"]["preview"]["diff"])
            deadline = time.monotonic() + 15
            done = waiting
            while done["status"] != "completed":
                if time.monotonic() > deadline:
                    raise TimeoutError("agent did not finish after approvals")
                if done["status"] in {"failed", "cancelled", "interrupted"}:
                    raise RuntimeError(done)
                for approval in done["agentRun"]["pendingApprovals"]:
                    call("approval/respond", {"taskId": task["id"], "approvalId": approval["id"],
                                              "decision": decision})
                time.sleep(0.01)
                done = call("task/get", {"taskId": task["id"]})
            assert done["agentRun"]["finished"] and not done["agentRun"]["outcomeUnknown"]
            report = json.loads(done["output"].removeprefix("ready"))
            if args.workspace:
                assert report["writeOk"] == (not args.deny)
                assert not report["directRead"]
                assert report["read"] == (None if args.deny else "你好\n")
                if args.deny:
                    assert not (directory / "via-agent.txt").exists()
                else:
                    assert (directory / "via-agent.txt").read_text(encoding="utf-8") == "one\n你好\nthree\n"
                print("Host file operations:", len(done["agentRun"]["hostTools"]))
            else:
                assert report["optionId"] == ("reject" if args.deny else "once")
            assert call("task/start", submission)["id"] == done["id"]
            events = call("events/list", {"taskId": task["id"]})["events"]
            print("Backend: ACP v1 / Windows AppContainer")
            print("Decision:", decision)
            print("Result:", done["status"], "/", done["agentRun"]["stopReason"])
            print("Output:", done["output"])
            print("Events:", " -> ".join(event["data"]["type"] for event in events))
        finally:
            child.stdin.close()
            try:
                child.wait(timeout=8)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
                raise RuntimeError("server did not finish agent cleanup") from None
            finally:
                reader.join(timeout=5)
                child.stdout.close()
            if child.returncode:
                raise RuntimeError(f"server exited with {child.returncode}")


if __name__ == "__main__":
    # Captured output on Windows must use the same UTF-8 as the RPC stream.
    sys.stdout.reconfigure(encoding="utf-8")
    main()
