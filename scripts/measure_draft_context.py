"""Measure request bytes and pre-dispatch rejection with a loopback model fixture."""

import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Thread
import time

from verify_installer_profile import connect


PROMPT = "What is the preferred project name? Give a one-word answer."
GUIDE = b"Project identifier: Cedar. The preferred name for this project is Cedar."
REPLY = {
    "id": "chatcmpl-fixture",
    "object": "chat.completion",
    "model": "context-fixture",
    "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "Cedar"}}],
}


@contextmanager
def fixture():
    seen = []
    response = json.dumps(REPLY, separators=(",", ":")).encode()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            if self.path != "/v1/chat/completions" or not 0 < length <= 1024 * 1024:
                self.send_error(400)
                return
            raw = self.rfile.read(length)
            body = json.loads(raw)
            seen.append({
                "generationBytes": len(raw),
                "toolCount": len(body.get("tools", [])),
                "hasGuide": any("Cedar" in str(msg.get("content", "")) for msg in body["messages"]),
            })
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = Thread(target=server.serve_forever)
        worker.start()
        try:
            yield server.server_address[1], seen
        finally:
            server.shutdown()
            worker.join()


def measure(binary, version, root, windows):
    project = root / "project"
    project.mkdir()
    (project / "AGENTS.md").write_bytes(GUIDE)
    config = root / "config.toml"
    measurements = []
    with fixture() as (port, seen):
        for window in windows:
            config.write_text(f"""[model]
provider = "openai"
protocol = "openai.compatible"
name = "context-fixture"
base_url = "http://127.0.0.1:{port}/v1/"
auth = "none"

[model.budget]
window = {window}
max_output_tokens = 128
""", encoding="utf-8")
            with connect(binary, root / f"window-{window}.db", version, config=config) as call:
                params = {"workspaceId": call("workspace/open", {"root": str(project)})["id"]}
                session = call("session/create", params)["id"]
                guide = call("workspace/previewProjectGuide", params)
                state = call("workspace/setProjectGuide", {
                    **params,
                    "expectedRevision": guide["state"]["revision"],
                    "sha256": guide["guide"]["sha256"],
                })
                offset = len(seen)
                task = call("task/start", {
                    "sessionId": session,
                    "input": PROMPT,
                    "context": {"projectGuideRevision": state["revision"], "enableInteraction": True},
                })["id"]
                # Small-window variants may either succeed or be rejected before HTTP dispatch.
                for _ in range(300):
                    result = call("task/get", {"taskId": task})
                    if result["status"] in ("completed", "failed"):
                        break
                    time.sleep(0.04)
                else:
                    raise RuntimeError(f"Task did not reach a terminal status: {task}")
                report = call("task/contextAttribution", {"taskId": task})
                rejection = report.get("latestBudgetRejection")
                report_data = rejection["data"]["report"] if rejection else None
                requests = seen[offset:]
                if result["status"] == "completed":
                    assert len(requests) == 1 and requests[0]["hasGuide"], requests
                else:
                    assert report_data is not None and not requests, (report, requests)
                measurements.append({
                    "windowTokensConfigured": window,
                    "status": result["status"],
                    "projectGuideBytes": report["projectGuide"]["bytes"],
                    "normalizedInputBytes": report["normalizedInputBytes"],
                    "wireRequests": requests,
                    "rejection": report_data,
                })
    return measurements


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="local fliuno-app-server executable")
    parser.add_argument("--version", required=True)
    parser.add_argument("--scratch", type=Path, default=Path.cwd(), help="existing scratch parent directory")
    parser.add_argument("--window", type=int, action="append", help="repeat to test multiple budgets")
    args = parser.parse_args()
    windows = args.window or [8192, 32768]
    if len(set(windows)) != len(windows) or any(not 128 < window <= 1000000 for window in windows):
        parser.error("window values must be distinct integers from 129 to 1000000")
    with TemporaryDirectory(dir=args.scratch, prefix="fliuno-draft-context-") as tmp:
        result = measure(args.binary.resolve(strict=True), args.version, Path(tmp), windows)
    print(json.dumps({"serverVersion": args.version, "measurements": result}, ensure_ascii=False, indent=2))
