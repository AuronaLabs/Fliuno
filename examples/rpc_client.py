"""Small stdio client for executable demos, with bounded waits and cleanup."""
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def build_binaries(*, fixtures=False):
    prepared = os.environ.get("FLIUNO_DEMO_BIN_DIR")
    if prepared:
        return Path(prepared).resolve(strict=True)
    command = ["cargo", "build", "--quiet", "--locked", "-p", "fliuno-app-server"]
    if fixtures:
        command += ["--features", "test-fixtures", "--bins"]
    subprocess.run(command, cwd=ROOT, check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT,
    ))
    return Path(metadata["target_directory"]) / "debug"


def build_server():
    name = "fliuno-app-server.exe" if os.name == "nt" else "fliuno-app-server"
    return (build_binaries() / name).resolve(strict=True)


class LocalKernel:
    def __init__(self, executable, arguments, environment):
        self.child = subprocess.Popen(
            [str(executable), *map(str, arguments)], cwd=ROOT, env=environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8",
        )
        self.replies = queue.Queue()
        self.request_id = 0
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        try:
            for line in self.child.stdout:
                self.replies.put(json.loads(line))
        finally:
            self.replies.put(None)

    def call(self, method, params, timeout=10):
        self.request_id += 1
        self.child.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": self.request_id, "method": method, "params": params,
        }) + "\n")
        self.child.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            response = self.replies.get(timeout=max(0, deadline - time.monotonic()))
            if response is None:
                raise RuntimeError(f"server exited with {self.child.poll()}")
            if "method" in response and "id" not in response:
                continue
            if response.get("id") != self.request_id or "error" in response:
                raise RuntimeError(response)
            return response["result"]

    def wait(self, task_id, expected):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            task = self.call("task/get", {"taskId": task_id})
            if task["status"] == expected:
                return task
            if task["status"] in {"completed", "failed", "cancelled", "interrupted"}:
                raise RuntimeError(task)
            time.sleep(0.01)
        raise TimeoutError(f"task did not reach {expected}")

    def __enter__(self):
        return self

    def __exit__(self, exception_type, _exception, _traceback):
        self.child.stdin.close()
        try:
            self.child.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait()
            if exception_type is None:
                raise RuntimeError("kernel did not finish cleanup") from None
        finally:
            self.reader.join(timeout=5)
            self.child.stdout.close()
        if self.child.returncode and exception_type is None:
            raise RuntimeError(f"kernel exited with {self.child.returncode}")
