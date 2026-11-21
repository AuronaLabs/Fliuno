"""Exercise the upstream A2A Testbed Task Runner through a local auth gate."""

import argparse
from collections import Counter
from contextlib import contextmanager
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import tempfile
import threading
import time

from rpc_client import LocalKernel, build_server

CARD = "/.well-known/agent-card.json"
LIMIT = 1 << 20


class RestartKernel(Exception):
    """Exit the stdio client after intentionally killing its server."""


@contextmanager
def gateway(port):
    token = secrets.token_hex(32)
    accepted, refused = Counter(), Counter()
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path == CARD:
                self.forward(None)
            else:
                self.send_error(404)

        def do_POST(self):
            if self.path != "/":
                return self.send_error(404)
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= LIMIT:
                return self.send_error(400)
            body = self.rfile.read(length)
            method = json.loads(body)["method"]
            if method not in {"SendMessage", "GetTask", "CancelTask"}:
                return self.send_error(400)
            assert self.headers["A2A-Version"] == "1.0"
            authorized = hmac.compare_digest(
                self.headers.get("Authorization", ""), f"Bearer {token}",
            )
            with lock:
                (accepted if authorized else refused)[method] += 1
            if not authorized:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Bearer realm="fliuno-interop"')
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.forward(body)

        def forward(self, body):
            upstream = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            try:
                headers = {}
                if body is not None:
                    headers.update({"Content-Type": "application/json", "A2A-Version": "1.0"})
                upstream.request(self.command, self.path, body=body, headers=headers)
                response = upstream.getresponse()
                payload = response.read(LIMIT + 1)
                if len(payload) > LIMIT:
                    raise ValueError("upstream response exceeds interop limit")
                if self.path == CARD and response.status == 200:
                    # Wrangler keeps the public custom domain in its local card.
                    card = json.loads(payload)
                    for interface in card["supportedInterfaces"]:
                        interface["url"] = f"http://127.0.0.1:{self.server.server_port}"
                    payload = json.dumps(card).encode("utf-8")
                self.send_response(response.status)
                self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            finally:
                upstream.close()

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}", token, accepted, refused
        finally:
            server.shutdown()
            thread.join(timeout=5)


def until(read, ready):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        value = read()
        if ready(value):
            return value
        time.sleep(0.02)
    raise TimeoutError(f"expected state was not observed: {value!r}")


def remote_check(kernel, task_id):
    result = kernel.call("task/checkRemote", {"taskId": task_id})

    def find_check():
        events = kernel.call("events/list", {"taskId": task_id})["events"]
        return next((item["data"] for item in events
                     if item["data"].get("checkId") == result["checkId"]
                     and item["data"]["type"] == "agent_remote_task_checked"), None)

    outcome = until(find_check, bool)["outcome"]
    assert outcome["status"] == "observed", outcome
    return outcome["state"]


def exercise(executable, root, base, token, accepted, refused):
    config = root / "agents.json"
    config.write_text(json.dumps({"agents": [{
        "id": "task-runner", "endpoint": base,
        "agentCardUrl": base + CARD, "bearerTokenEnv": "FLIUNO_TEST_A2A_TOKEN",
        "pollIntervalMs": 25, "requestTimeoutMs": 4000, "taskTimeoutMs": 8000,
    }]}), encoding="utf-8")

    def open_kernel(name, credential):
        return LocalKernel(executable, ["--db", root / name, "--a2a-config", config],
                           dict(os.environ, FLIUNO_PROVIDER="demo", FLIUNO_TEST_A2A_TOKEN=credential))

    def initialize(kernel):
        kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "a2a-task-runner-interop"})

    def session(kernel):
        workspace = kernel.call("workspace/open", {"root": str(root)})
        return kernel.call("session/create", {"workspaceId": workspace["id"]})["id"]

    def request(session_id, request_id, prompt):
        return {"sessionId": session_id, "input": prompt, "clientRequestId": request_id,
                "backend": {"type": "external_agent", "agentId": "task-runner"}}

    def get(kernel, task_id):
        return kernel.call("task/get", {"taskId": task_id})

    def finished(task):
        return task.get("agentRun", {}).get("finished")

    def working(kernel, task_id):
        return until(lambda: get(kernel, task_id),
                     lambda task: task.get("agentRun", {}).get("remoteTask"))

    with open_kernel("denied.db", "wrong-token") as kernel:
        initialize(kernel)
        task_id = kernel.call("task/start", request(session(kernel), "denied", "count: 1"))["id"]
        rejected = until(lambda: get(kernel, task_id), finished)
        assert rejected["status"] != "completed" and not rejected["agentRun"].get("remoteTask"), rejected
    assert refused == {"SendMessage": 1} and not accepted, (refused, accepted)

    with open_kernel("work.db", token) as kernel:
        initialize(kernel)
        session_id = session(kernel)
        prompt = "count: 3 Fliuno interop"
        submitted = request(session_id, "complete", prompt)
        task_id = kernel.call("task/start", submitted)["id"]
        completed = until(lambda: get(kernel, task_id), finished)
        assert completed["status"] == "completed", completed
        assert completed["agentRun"]["outcomeUnknown"] is False, completed
        artifacts = kernel.call("artifacts/list", {"taskId": task_id})
        assert sorted(json.loads(item["content"])["parts"][0]["text"] for item in artifacts) == [
            f"unit {i}/3: {prompt}" for i in range(1, 4)
        ], artifacts
        assert accepted["SendMessage"] == 1 and accepted["GetTask"] > 0, accepted

    with open_kernel("work.db", token) as kernel:
        initialize(kernel)
        assert kernel.call("task/start", submitted)["id"] == task_id
        assert get(kernel, task_id) == completed
        assert remote_check(kernel, task_id) == "completed"
        assert accepted["SendMessage"] == 1, accepted

        cancelled_id = kernel.call("task/start", request(session_id, "cancel", "count: 20"))["id"]
        running = working(kernel, cancelled_id)
        assert running["agentRun"]["remoteTask"]["state"] == "working", running
        kernel.call("task/cancel", {"taskId": cancelled_id})
        cancelled = until(lambda: get(kernel, cancelled_id), finished)
        assert cancelled["status"] == "cancelled", cancelled
        assert cancelled["agentRun"]["outcomeUnknown"] is False, cancelled
        assert remote_check(kernel, cancelled_id) == "cancelled"
        assert accepted["SendMessage"] == 2 and accepted["CancelTask"] == 1, accepted

    during_run = request(session_id, "hard-restart", "count: 20")
    try:
        with open_kernel("work.db", token) as kernel:
            initialize(kernel)
            interrupted_id = kernel.call("task/start", during_run)["id"]
            running = working(kernel, interrupted_id)
            assert not running["agentRun"]["finished"], running
            remote_id = running["agentRun"]["remoteTask"]["id"]
            kernel.child.kill()
            kernel.child.wait(timeout=5)
            raise RestartKernel()
    except RestartKernel:
        pass

    with open_kernel("work.db", token) as kernel:
        initialize(kernel)
        recovered = get(kernel, interrupted_id)
        assert recovered["status"] == "interrupted", recovered
        assert recovered["agentRun"]["outcomeUnknown"] is True, recovered
        assert recovered["agentRun"]["remoteTask"]["id"] == remote_id, recovered
        assert kernel.call("task/start", during_run)["id"] == interrupted_id
        deadline = time.monotonic() + 6
        while True:
            state = remote_check(kernel, interrupted_id)
            if state == "completed":
                break
            assert state == "working" and time.monotonic() < deadline, state
            time.sleep(0.1)
        assert get(kernel, interrupted_id) == recovered
        assert accepted["SendMessage"] == 3 and accepted["CancelTask"] == 1, accepted
    print("A2A Task Runner: 401 before forwarding, one submit per task, artifacts, hard restart, cancel")
    print(f"Forwarded methods: {dict(accepted)}; rejected methods: {dict(refused)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", required=True, type=int, help="Local upstream Wrangler dev port")
    args = parser.parse_args()
    executable = build_server()
    with tempfile.TemporaryDirectory(prefix="fliuno-task-runner-") as directory, gateway(args.port) as gate:
        exercise(executable, Path(directory), *gate)


if __name__ == "__main__":
    main()
