"""Exercise named suppliers against the same database migrated from an installer."""
from argparse import ArgumentParser
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
from queue import Queue
from tempfile import TemporaryDirectory
from threading import Thread

from verify_installer_profile import CREDENTIAL, connect, seed, verify, wait_for


def reply_stream(claude, reply):
    if claude:
        events = [
            {"type": "message_start", "message": {"content": [], "usage": {"input_tokens": 3}}},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": reply}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}},
            {"type": "message_stop"},
        ]
        return "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events)
    events = [
        {"choices": [{"index": 0, "delta": {"content": reply}, "finish_reason": None}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
    ]
    return "".join(f"data: {json.dumps(event)}\n\n" for event in events) + "data: [DONE]\n\n"


@contextmanager
def endpoint(protocol, key, model):
    claude = protocol == "claude.message"
    path = "/v1/messages" if claude else "/v1/chat/completions"
    header, value, other = (("x-api-key", key, "Authorization") if claude else
                            ("Authorization", f"Bearer {key}", "x-api-key"))
    received = Queue()
    stream = reply_stream(claude, f"Reply from {model}").encode("utf-8")

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            ok = (self.path == path and self.headers.get(header) == value
                  and self.headers.get(other) is None and request.get("model") == model
                  and request.get("stream") is True)
            received.put(ok)
            if not ok:
                self.send_error(400, "Fixture route, protocol or credential mismatch")
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(stream)))
            self.end_headers()
            self.wfile.write(stream)

        def log_message(self, *_args):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1/", received
        finally:
            server.shutdown()
            worker.join()


def run(args, scratch):
    directory = scratch / "migrated"
    seed(directory, args.baseline, args.from_version)
    verify(directory, args.candidate, args.to_version)
    fixture = json.loads((directory / "fixture.json").read_text(encoding="utf-8"))
    home = directory / ".fliuno"
    config, database = home / "config.toml", home / "state/workbench.db"
    # Capture a still-pending action from the old tool registration, then open
    # it with the new server. Its approval must not produce a new artifact.
    with connect(args.baseline, database, args.from_version) as call:
        session = call("session/create", {"workspaceId": fixture["workspace"], "title": "Pending migration"})
        task = call("task/start", {"sessionId": session["id"], "input": "Create a pending artifact."})
        pending = wait_for(call, task["id"], "waiting_approval")["pendingApproval"]
        assert pending["call"]["name"] == "artifact_create"
    with connect(args.candidate, database, args.to_version) as call:
        call("approval/respond", {"taskId": task["id"], "approvalId": pending["id"], "decision": "allow_once"})
        failed = wait_for(call, task["id"], "failed")
        assert "registration changed" in failed["output"], failed
        assert not call("artifacts/listSummaries", {"taskId": task["id"]})["items"]
    keys = [CREDENTIAL.decode().strip(), "second-supplier-fixture-key"]
    (home / "credentials/second.key").write_text(keys[1] + "\n", encoding="ascii")
    routes = [("lab", "openai.compatible", "lab-model", "openai.key"),
              ("review", "claude.message", "review-model", "second.key")]
    tasks = []
    with endpoint(routes[0][1], keys[0], routes[0][2]) as lab, \
         endpoint(routes[1][1], keys[1], routes[1][2]) as review:
        with connect(args.candidate, database, args.to_version, config=config) as call:
            old = call("task/get", {"taskId": fixture["task"]})
            revision = call("modelSuppliers/list", {})["revision"]
            for (name, protocol, model, filename), (base, requests) in zip(routes, (lab, review)):
                saved = call("modelSuppliers/save", {
                    "target": {"type": "new", "supplier": name}, "revision": revision,
                    "preset": "custom", "baseUrl": base,
                    "credential": {"type": "file", "path": f"credentials/{filename}"},
                })
                bound = call("modelProfiles/bindSupplier", {
                    "target": {"type": "new", "profile": name}, "revision": saved["revision"],
                    "supplier": name, "protocol": protocol, "model": model, "reasoningEffort": None,
                })
                revision = bound["revision"]
                assert all(key not in json.dumps(call("modelSuppliers/list", {})) for key in keys)
                assert call("modelProfiles/select", {"profile": name, "reload": True})["model"] == model
                task = call("task/start", {"sessionId": fixture["session"], "input": f"Reply using {name}."})
                wait_for(call, task["id"], "completed")
                assert requests.get(timeout=2) and requests.empty(), "Unexpected supplier generation request"
                tasks.append(task["id"])
            call("modelProfiles/setDefault", {"profile": "review"})
            assert call("modelProfiles/getDefault", {})["profile"] == "review"
        with connect(args.candidate, database, args.to_version, config=config) as call:
            assert call("modelProfiles/list", {})["active"]["profile"] == "review"
            assert call("task/get", {"taskId": fixture["task"]}) == old
            items = call("session/listHistory", {"sessionId": fixture["session"]})["entries"]
            history = {entry["turn"]["taskId"]: entry["turn"]["messages"] for entry in items
                       if entry["kind"] == "turn"}
            for (name, _, model, _), task in zip(routes, tasks):
                replies = [m for m in history[task] if m["role"] == "assistant"]
                assert replies == [{"role": "assistant", "text": f"Reply from {model}",
                                    "identity": {"supplier": name, "model": model}}]
            assert all(key not in json.dumps(history) for key in keys)
    assert all(key not in config.read_text(encoding="utf-8") for key in keys)
    print(f"Verified {args.from_version} -> {args.to_version}: stale approval blocked; two supplier routes, credentials, default and history")


if __name__ == "__main__":
    parser = ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path, help="app-server from the verified baseline archive")
    parser.add_argument("candidate", type=Path, help="candidate app-server with its sibling fliuno")
    parser.add_argument("--from-version", required=True)
    parser.add_argument("--to-version", required=True)
    parser.add_argument("--scratch", required=True, type=Path, help="existing parent directory for a temporary profile")
    options = parser.parse_args()
    options.baseline = options.baseline.resolve(strict=True)
    options.candidate = options.candidate.resolve(strict=True)
    with TemporaryDirectory(prefix="fliuno-supplier-upgrade-", dir=options.scratch.resolve(strict=True)) as temporary:
        run(options, Path(temporary))
