#!/usr/bin/env python3
"""Compare 32-session archive batches against two real stdio app servers."""

import argparse
from contextlib import ExitStack
from pathlib import Path
import random
import statistics
from tempfile import TemporaryDirectory
from time import perf_counter_ns
import os
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "examples"))
from rpc_client import LocalKernel

SESSION_COUNT = 32
WARMUP_PAIRS = 12
MEASURED_PAIRS = 80


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path, help="unmodified app-server executable")
    parser.add_argument("current", type=Path, help="optimized app-server executable")
    args = parser.parse_args()
    executables = [args.baseline.resolve(strict=True), args.current.resolve(strict=True)]

    with TemporaryDirectory(prefix="fliuno-session-archive-benchmark-") as temporary:
        with ExitStack() as stack:
            kernels = []
            expected = []
            states = []
            environment = dict(os.environ, FLIUNO_PROVIDER="demo")
            for index, executable in enumerate(executables):
                root = Path(temporary) / str(index)
                root.mkdir()
                kernel = stack.enter_context(LocalKernel(
                    executable, ["--db", root / "kernel.db"], environment,
                ))

                def rpc(method, kernel=kernel, **params):
                    return kernel.call(method, params)

                info = rpc("initialize", protocolVersion="0.1", clientName="archive-benchmark")
                assert info["capabilities"]["maxSessionArchiveBatch"] >= SESSION_COUNT
                workspace_id = rpc("workspace/open", root=str(root))["id"]
                names = [f"session-{number:02}" for number in range(SESSION_COUNT)]
                sessions = [
                    rpc("session/create", workspaceId=workspace_id, title=name)
                    for name in names
                ]
                kernels.append(rpc)
                expected.append([(item["id"], item["title"]) for item in sessions])
                states.append(False)

            measurements = [[], []]

            def archive(index, archived, timed):
                started = perf_counter_ns()
                response = kernels[index](
                    "session/setArchivedBatch",
                    sessionIds=[item[0] for item in expected[index]],
                    archived=archived,
                )["sessions"]
                elapsed = (perf_counter_ns() - started) / 1e6
                assert len(response) == SESSION_COUNT
                assert [(item["id"], item["title"]) for item in response] == expected[index]
                assert all(item.get("archived", False) is archived for item in response)
                states[index] = archived
                if timed:
                    measurements[index].append(elapsed)

            for index in range(2):
                archive(index, True, False)
                archive(index, False, False)

            order = [0, 1] * (WARMUP_PAIRS + MEASURED_PAIRS)
            random.Random(715).shuffle(order)
            for pair, first in enumerate(order):
                for index in (first, 1 - first):
                    archive(index, True, pair >= 2 * WARMUP_PAIRS)
                    archive(index, False, pair >= 2 * WARMUP_PAIRS)

            print(f"{SESSION_COUNT} sessions per transaction, {MEASURED_PAIRS} paired iterations")
            for label, samples in zip(("baseline", "current"), measurements):
                print(
                    f"{label}: median {statistics.median(samples):.3f} ms, "
                    f"p95 {statistics.quantiles(samples, n=20)[18]:.3f} ms"
                )
            print(
                "Both servers returned all session IDs and titles in request order "
                "for archive and unarchive batches."
            )


if __name__ == "__main__":
    main()
