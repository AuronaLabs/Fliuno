#!/usr/bin/env python3
"""Measure task-time memory injection through an actual stdio app server."""

import argparse
from contextlib import closing, contextmanager, nullcontext
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import random
import sqlite3
import statistics
import sys
import tempfile
import threading
from time import monotonic, perf_counter_ns, sleep

SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS.parent / "examples"))
from rpc_client import LocalKernel

MODEL = "multilingual-e5-small"
DIMENSIONS = 384
MODEL_ID = "e5-small:614241f622f53c4eeff9890bdc4f31cfecc418b3:qint8"
WEIGHTS_SHA256 = "dd476dd0c2514e9b9be83aeb3853fac0763e0bdf4a71645407587d77c48a2d88"
TOKENIZER_SHA256 = "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39"


class Embedder:
    def __init__(self, weights, tokenizer):
        import numpy
        import onnxruntime
        from tokenizers import Tokenizer

        for path, expected in [(weights, WEIGHTS_SHA256), (tokenizer, TOKENIZER_SHA256)]:
            with path.open("rb") as model_file:
                actual = hashlib.file_digest(model_file, "sha256").hexdigest()
            if actual != expected:
                raise ValueError(f"SHA-256 mismatch for {path.name}; use the pinned model files")
        self.np = numpy
        self.tokenizer = Tokenizer.from_file(str(tokenizer))
        self.tokenizer.enable_padding(pad_id=1, pad_token="<pad>")
        self.tokenizer.enable_truncation(max_length=512)
        options = onnxruntime.SessionOptions()
        options.intra_op_num_threads = 2
        options.inter_op_num_threads = 1
        self.session = onnxruntime.InferenceSession(
            str(weights), sess_options=options, providers=["CPUExecutionProvider"],
        )
        self.inputs = {item.name for item in self.session.get_inputs()}

    def vectors(self, inputs):
        # Fliuno sends indexed passages as key + newline + body; eval queries have no newlines.
        text = [("passage: " if "\n" in item else "query: ") + item for item in inputs]
        encoded = self.tokenizer.encode_batch(text)
        ids = self.np.array([item.ids for item in encoded], dtype=self.np.int64)
        mask = self.np.array([item.attention_mask for item in encoded], dtype=self.np.int64)
        fields = {"input_ids": ids, "attention_mask": mask, "token_type_ids": self.np.zeros_like(ids)}
        output = self.session.run(["last_hidden_state"], {name: fields[name] for name in self.inputs})[0]
        pooled = (output * mask[..., None]).sum(axis=1) / mask.sum(axis=1, keepdims=True)
        vectors = pooled / self.np.linalg.norm(pooled, axis=1, keepdims=True)
        assert vectors.shape == (len(inputs), DIMENSIONS)
        return vectors.tolist()


@contextmanager
def embedding_server(embedder):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            if self.path != "/embeddings" or not 0 < length <= 1 << 20:
                return self.send_error(400)
            request = json.loads(self.rfile.read(length))
            assert request["model"] == MODEL and request["dimensions"] == DIMENSIONS
            data = embedder.vectors(request["input"])
            response = json.dumps({"data": [{"index": i, "embedding": row}
                                            for i, row in enumerate(data)]}).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/embeddings"
        finally:
            server.shutdown()
            thread.join(timeout=5)


def wait_for_index(db, count):
    deadline = monotonic() + 10
    with closing(sqlite3.connect(db)) as conn:
        while monotonic() < deadline:
            actual = conn.execute("SELECT count(*) FROM workspace_memory_vectors").fetchone()[0]
            if actual == count:
                return
            sleep(0.05)
    raise TimeoutError("embedding index did not populate")


def evaluate(executable, cases, endpoint, minimum, repetitions):
    with tempfile.TemporaryDirectory(prefix="fliuno-memory-eval-") as temporary:
        root = Path(temporary)
        db = root / "eval.db"
        environment = dict(os.environ, FLIUNO_PROVIDER="demo")

        def initialize(arguments):
            return LocalKernel(executable, ["--db", db, *arguments], environment)

        def handshake(kernel):
            return kernel.call("initialize", {"protocolVersion": "0.1", "clientName": "memory-eval"})

        with initialize([]) as kernel:
            handshake(kernel)
            workspace = kernel.call("workspace/open", {"root": str(root)})["id"]
            for key, content in cases["memories"].items():
                kernel.call("workspace/memory/put", {"workspaceId": workspace, "key": key, "content": content})

        arguments = []
        if endpoint is not None:
            config = root / "embedding.json"
            config.write_text(json.dumps({
                "endpoint": endpoint, "model": MODEL, "modelIdentity": MODEL_ID,
                "dimensions": DIMENSIONS, "workspaces": [workspace], "minimumSimilarity": minimum,
            }), encoding="utf-8")
            arguments.extend(["--memory-embedding-config", config])

        samples = []
        injected = {}
        order = list(cases["queries"])
        randomizer = random.Random(5)
        with initialize(arguments) as kernel:
            capabilities = handshake(kernel)["capabilities"]
            if endpoint is not None:
                assert capabilities["taskMemoryHybridRetrieval"] is True
                wait_for_index(db, len(cases["memories"]))
            for iteration in range(repetitions + 1):
                randomizer.shuffle(order)
                for text, expected in order:
                    session = kernel.call("session/create", {"workspaceId": workspace})["id"]
                    start = perf_counter_ns()
                    task = kernel.call("task/start", {"sessionId": session, "input": text})
                    elapsed = (perf_counter_ns() - start) / 1_000_000
                    context = task["context"]
                    keys = sorted(entry["key"] for entry in context.get("memories", []))
                    result = {"expected": sorted(expected), "actual": keys,
                              "fallback": context.get("memoryFallback")}
                    assert injected.setdefault(text, result) == result, (text, result)
                    if iteration > 0:
                        samples.append(elapsed)
                    # The demo provider requests approval; only task creation is timed.
                    stopped = kernel.call("task/cancel", {"taskId": task["id"]})
                    assert stopped["status"] == "cancelled", stopped
        return {
            "positive_queries": sum(bool(row["expected"]) for row in injected.values()),
            "hits": sum(bool(set(row["expected"]) & set(row["actual"])) for row in injected.values()),
            "queries_with_false_injection": sum(bool(set(row["actual"]) - set(row["expected"]))
                                                for row in injected.values()),
            "queries": len(injected), "samples": len(samples),
            "p50_ms": round(statistics.median(samples), 3),
            "p95_ms": round(sorted(samples)[math.ceil(0.95 * len(samples)) - 1], 3),
            "details": injected,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--split", choices=["calibration", "holdout"], required=True)
    parser.add_argument("--cases", type=Path, default=SCRIPTS / "memory_eval_cases.json")
    parser.add_argument("--weights", type=Path)
    parser.add_argument("--tokenizer", type=Path)
    parser.add_argument("--minimum-similarity", type=float)
    parser.add_argument("--repetitions", type=int, default=8)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("--repetitions must be positive")
    flags = [args.weights, args.tokenizer, args.minimum_similarity]
    if any(flag is not None for flag in flags) and not all(flag is not None for flag in flags):
        parser.error("provide --weights, --tokenizer and --minimum-similarity together")
    cases = json.loads(args.cases.read_text(encoding="utf-8"))[args.split]
    executable = args.executable.resolve(strict=True)
    server = embedding_server(Embedder(args.weights, args.tokenizer)) if args.weights else nullcontext()
    with server as endpoint:
        result = evaluate(executable, cases, endpoint, args.minimum_similarity, args.repetitions)
    print(json.dumps({"split": args.split, "minimum_similarity": args.minimum_similarity, **result}, indent=2))


if __name__ == "__main__":
    main()
