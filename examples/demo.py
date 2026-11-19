"""Offline process demo: submit -> approve once -> artifact -> event replay.

Uses a temporary database and forces the demo provider. No model API calls.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from rpc_client import build_server

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deny", action="store_true", help="Reject the proposed artifact instead")
    args = parser.parse_args()
    exe = build_server()
    with tempfile.TemporaryDirectory(prefix="fliuno-demo-") as directory:
        env = dict(os.environ, FLIUNO_PROVIDER="demo")
        child = subprocess.Popen([str(exe), "--db", str(Path(directory) / "kernel.db")],
                                 cwd=ROOT, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 text=True, encoding="utf-8")
        request_id = 0
        notifications = []

        def call(method, params):
            nonlocal request_id
            request_id += 1
            child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
            child.stdin.flush()
            while True:
                line = child.stdout.readline()
                if not line:
                    raise RuntimeError(f"server exited with {child.poll()}")
                response = json.loads(line)
                if response.get("id") != request_id:
                    if "method" in response and "id" not in response:
                        notifications.append(response)
                        continue
                    raise RuntimeError(response)
                if "error" in response:
                    raise RuntimeError(response)
                return response["result"]

        def wait_for_status(task_id, expected):
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                task = call("task/get", {"taskId": task_id})
                if task["status"] == expected:
                    return task
                if task["status"] in {"failed", "cancelled", "interrupted"}:
                    raise RuntimeError(f"unexpected task status: {task}")
                time.sleep(0.01)
            raise TimeoutError(f"task did not reach {expected}")

        try:
            call("initialize", {"protocolVersion": "0.1", "clientName": "offline-demo"})
            ws = call("workspace/open", {"root": str(ROOT)})
            session = call("session/create", {"workspaceId": ws["id"]})
            task = call("task/start", {"sessionId": session["id"], "input": "Fliuno offline artifact: review, approve, persist."})
            task = wait_for_status(task["id"], "waiting_approval")
            print("Task:", task["status"])
            print("Proposed tool:", task["pendingApproval"]["call"]["name"])
            assert call("artifacts/list", {"taskId": task["id"]}) == []
            task = call("approval/respond", {"taskId": task["id"], "approvalId": task["pendingApproval"]["id"],
                                             "decision": "deny" if args.deny else "allow_once"})
            task = wait_for_status(task["id"], "failed" if args.deny else "completed")
            artifacts = call("artifacts/list", {"taskId": task["id"]})
            assert task["status"] == ("failed" if args.deny else "completed")
            assert len(artifacts) == (0 if args.deny else 1)
            events = call("events/list", {"taskId": task["id"]})
            print("Result:", task["status"])
            print("Artifacts:", len(artifacts))
            print("Events:", " -> ".join(event["data"]["type"] for event in events["events"]))
        finally:
            child.stdin.close()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            child.stdout.close()


if __name__ == "__main__":
    main()
