"""Run the local Rust HTTP host, approve one task, and resume from an SSE frame."""
import argparse
import http.client
import json
import os
from pathlib import Path
import queue
import secrets
import subprocess
import tempfile
import threading

from rpc_client import ROOT, build_server


PNG = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yhE0A"
    "AAAASUVORK5CYII="
)
PDF = (
    "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2Jq"
    "CjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2Jq"
    "CjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCAxMCAx"
    "MF0gPj4KZW5kb2JqCnhyZWYKMCA0CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAw"
    "MCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKdHJhaWxlcgo8PCAv"
    "U2l6ZSA0IC9Sb290IDEgMCBSID4+CnN0YXJ0eHJlZgoxODQKJSVFT0YK"
)


def build_host(binary_directory):
    if not os.environ.get("FLIUNO_DEMO_BIN_DIR"):
        subprocess.run(["cargo", "build", "--quiet", "--locked", "-p", "fliuno-ag-ui-http",
                        "--example", "local_host"], cwd=ROOT, check=True)
    name = "local_host.exe" if os.name == "nt" else "local_host"
    return (binary_directory / "examples" / name).resolve(strict=True)


def frames(response):
    event_id = None
    until_blank = None
    while line := response.readline():
        line = line.decode("utf-8").rstrip("\r\n")
        if line.startswith("id: "):
            event_id = line[4:]
        elif line.startswith("data: "):
            until_blank = json.loads(line[6:])
        elif not line and event_id is not None and until_blank is not None:
            yield event_id, until_blank
            event_id, until_blank = None, None


def exercise(address, session, run_token, review_token, *, interrupts=False, inline_media=False):
    def request(connection, method, path, token, body=None, extra=None):
        headers = {"Authorization": f"Bearer {token}", **(extra or {})}
        if body is not None:
            headers["Content-Type"] = "application/json"
            body = json.dumps(body).encode("utf-8")
        connection.request(method, path, body, headers)
        return connection.getresponse()

    content = "Prepare an artifact."
    expected_text = content
    if inline_media:
        content = [
            {"type": "text", "text": "Review "},
            {"type": "image", "source": {"type": "data", "mimeType": "image/png", "value": PNG}},
            {"type": "text", "text": " and "},
            {"type": "document", "source": {"type": "data", "mimeType": "application/pdf", "value": PDF}},
        ]
        expected_text = "Review [image] and [document]"

    run = http.client.HTTPConnection(address, timeout=10)
    control = http.client.HTTPConnection(address, timeout=10)
    def review(approval_id):
        details = request(control, "GET", f"/runs/offline-http-demo/approvals/{approval_id}", review_token)
        assert details.status == 200
        approval = json.load(details)["approval"]
        assert approval["call"]["arguments"]["content"] == expected_text

    try:
        response = request(run, "POST", "/run", run_token, {
            "threadId": session, "runId": "offline-http-demo",
            "messages": [{"id": "message-1", "role": "user", "content": content}],
        })
        assert response.status == 200 and response.headers["Content-Type"] == "text/event-stream"
        run_id = "offline-http-demo"
        if interrupts:
            parent = list(frames(response))
            assert parent[-1][1]["type"] == "RUN_FINISHED"
            outcome = parent[-1][1]["outcome"]
            assert outcome["type"] == "interrupt"
            approval_id = outcome["interrupts"][0]["id"]
            review(approval_id)
            run_id = "offline-http-child"
            continuation = {"threadId": session, "runId": run_id, "parentRunId": "offline-http-demo",
                            "messages": [], "resume": [{"interruptId": approval_id,
                                                       "status": "resolved", "payload": {"decision": "allow_once"}}]}
            rejected = request(control, "POST", "/run", run_token, continuation)
            assert rejected.status == 401
            rejected.read()
            resumed = request(run, "POST", "/run", review_token, continuation)
            assert resumed.status == 200 and resumed.headers["Content-Type"] == "text/event-stream"
            delivered = list(frames(resumed))
            assert delivered[0][1]["runId"] == run_id
            original = request(control, "GET", "/runs/offline-http-demo", run_token)
            assert original.status == 200 and list(frames(original)) == parent
        else:
            delivered = []
            for event_id, event in frames(response):
                delivered.append((event_id, event))
                if event.get("name") == "fliuno.approval_requested":
                    approval = event["value"]["approvalId"]
                    if inline_media:
                        review(approval)
                    path = f"/runs/offline-http-demo/approvals/{approval}"
                    rejected = request(control, "POST", path, run_token, {"decision": "allow_once"})
                    assert rejected.status == 401
                    rejected.read()
                    accepted = request(control, "POST", path, review_token, {"decision": "allow_once"})
                    assert accepted.status == 200
                    assert json.load(accepted)["data"]["decision"] == "allow_once"
        assert delivered[-1][1]["type"] == "RUN_FINISHED"
        if inline_media:
            all_frames = parent + delivered if interrupts else delivered
            for _, event in all_frames:
                encoded_event = json.dumps(event)
                assert PNG not in encoded_event and PDF not in encoded_event
            print(f"Demo approval text: {expected_text}")
        position = next(index for index, (_, event) in enumerate(delivered)
                        if event["type"] == "TEXT_MESSAGE_CONTENT")
        response = request(control, "GET", f"/runs/{run_id}", run_token,
                           extra={"Last-Event-ID": delivered[position][0]})
        assert response.status == 200
        assert list(frames(response)) == delivered[position + 1:]
        profile = "interrupt" if interrupts else "legacy"
        print(f"AG-UI HTTP {profile} profile: reviewer authorization and frame-level replay passed.")
    finally:
        control.close()
        run.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--interrupts", action="store_true", help="Resume a standard approval interrupt with a new run ID")
    parser.add_argument("--inline-media", action="store_true", help="Submit a tiny PNG and PDF in the same message")
    args = parser.parse_args()
    server = build_server()
    host = build_host(server.parent)
    run_token, review_token = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
    with tempfile.TemporaryDirectory(prefix="fliuno-http-demo-") as directory:
        env = dict(os.environ, FLIUNO_DEMO_RUN_TOKEN=run_token, FLIUNO_DEMO_REVIEW_TOKEN=review_token)
        env.pop("FLIUNO_DEMO_INTERRUPTS", None)
        if args.interrupts:
            env["FLIUNO_DEMO_INTERRUPTS"] = "1"
        child = subprocess.Popen([str(host), str(server), directory, str(Path(directory) / "kernel.db")],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                                 encoding="utf-8", env=env, cwd=ROOT)
        try:
            ready = queue.Queue(maxsize=1)
            threading.Thread(target=lambda: ready.put(child.stdout.readline()), daemon=True).start()
            launch = json.loads(ready.get(timeout=15))
            exercise(launch["address"], launch["sessionId"], run_token, review_token,
                     interrupts=args.interrupts, inline_media=args.inline_media)
        finally:
            child.stdin.close()
            try:
                child.wait(timeout=8)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
                raise
            finally:
                child.stdout.close()
        if child.returncode:
            raise RuntimeError(f"local AG-UI host exited with {child.returncode}")


if __name__ == "__main__":
    main()
