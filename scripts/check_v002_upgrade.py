#!/usr/bin/env python3
"""Verify an upgrade from v0.0.2 with two real stdio app-server binaries."""

import argparse
from base64 import b64decode
from contextlib import closing, contextmanager
from hashlib import sha256
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "examples"))
from rpc_client import LocalKernel


@contextmanager
def server(executable, db, version):
    environment = dict(os.environ, FLIUNO_PROVIDER="demo")
    with LocalKernel(executable, ["--db", db], environment) as kernel:
        def rpc(method, **params):
            return kernel.call(method, params)

        info = rpc("initialize", protocolVersion="0.1", clientName="tag-upgrade-smoke")
        assert info["serverVersion"] == version, info
        yield rpc, kernel.wait


def event_keys(page):
    return [
        (event["sequence"], event["taskId"], event["data"]["type"])
        for event in page["events"]
    ]


def event_rows(sql, through):
    return sql.execute(
        "SELECT sequence, task_id, CAST(data AS BLOB) FROM events "
        "WHERE sequence <= ? ORDER BY sequence",
        (through,),
    ).fetchall()


def check_upgrade(old, current, root):
    db = root / "kernel.db"
    report_text = "旧版本交付物\nUTF-8 content\n"
    report_body = report_text.encode("utf-8")
    report_hash = sha256(report_body).hexdigest()
    request_ids = ["old-completed", "old-pending"]

    # Create both a completed task and a durable pending operation with the release.
    with server(old, db, "0.0.2") as (rpc, wait):
        workspace = rpc("workspace/open", root=str(root))["id"]
        session = rpc("session/create", workspaceId=workspace, title="旧会话")["id"]
        completed = rpc(
            "task/start", sessionId=session, input=report_text, clientRequestId=request_ids[0],
        )["id"]
        approval = wait(completed, "waiting_approval")["pendingApproval"]["id"]
        rpc("approval/respond", taskId=completed, approvalId=approval, decision="allow_once")
        wait(completed, "completed")
        [item] = rpc("artifacts/list", taskId=completed)
        report = {"taskId": completed, "artifactId": item["id"]}
        assert rpc("artifacts/digest", **report)["sha256"] == report_hash

        task = rpc(
            "task/start", sessionId=session, input="待审批旧任务", clientRequestId=request_ids[1],
        )["id"]
        pending = {
            "taskId": task,
            "approvalId": wait(task, "waiting_approval")["pendingApproval"]["id"],
        }
        saved_detail = rpc("approval/get", **pending)
        assert saved_detail["kind"] == "tool_call"
        rpc("session/setArchived", sessionId=session, archived=True)
        original = rpc("events/listAll", after=0, limit=100)
        cut = original["nextAfter"]
        assert cut > 0 and not original["hasMore"]

    with closing(sqlite3.connect(db)) as sql:
        before = sql.execute("PRAGMA user_version").fetchone()[0]
        assert before == 36, before
        assert sql.execute("SELECT MAX(sequence) FROM events").fetchone() == (cut,)
        original_rows = event_rows(sql, cut)

    # Verify state before resolving the pending operation through the new binary.
    with server(current, db, "0.0.3") as (rpc, wait):
        assert rpc("session/get", sessionId=session)["archived"] is True
        assert rpc("task/getSummary", taskId=completed)["status"] == "completed"
        assert rpc("approval/get", **pending) == saved_detail
        assert rpc("approval/getBatch", approvals=[pending])["items"] == [saved_detail]
        assert rpc("artifacts/digest", **report)["sha256"] == report_hash
        body = rpc("artifacts/read", **report, after=0, limit=1000)
        assert b64decode(body["dataBase64"], validate=True) == report_body

        saved = rpc("events/listAll", after=0, through=cut, limit=100)
        assert event_keys(saved) == event_keys(original)
        assert saved["nextAfter"] == cut and not saved["hasMore"]
        counts = rpc("work/getSessionCounts", workspaceId=workspace, sessionIds=[session])
        assert counts["throughSequence"] == cut
        [entry] = counts["items"]
        assert entry["taskCounts"]["completed"] == 1
        assert entry["taskCounts"]["waitingApproval"] == 1
        matches = rpc("task/getSummariesByRequest", sessionId=session, clientRequestIds=request_ids)
        assert [item["id"] for item in matches["items"]] == [completed, task]

        rpc("approval/respond", **pending, decision="allow_once")
        wait(task, "completed")
        replay = rpc("events/listAll", after=cut, limit=100)
        end = replay["nextAfter"]
        assert end > cut and replay["events"] and not replay["hasMore"]
        for event in replay["events"]:
            assert event["sequence"] > cut and event["taskId"] == task, event
        [new_artifact] = rpc("artifacts/list", taskId=task)

    with closing(sqlite3.connect(db)) as sql:
        after = sql.execute("PRAGMA user_version").fetchone()[0]
        assert after >= 52, after
        assert event_rows(sql, cut) == original_rows
        assert sql.execute("PRAGMA integrity_check").fetchone() == ("ok",)

    with server(current, db, "0.0.3") as (rpc, _):
        assert rpc("approval/getBatch", approvals=[pending])["items"] == [None]
        assert rpc("task/getSummary", taskId=task)["status"] == "completed"
        assert rpc("events/listAll", after=end, limit=100)["events"] == []
        assert rpc("artifacts/list", taskId=task) == [new_artifact]

    print(f"v0.0.2 -> v0.0.3 SQLite {before} -> {after}")
    print(f"Preserved {len(original_rows)} old event rows byte for byte.")
    print("Preserved the archived session, request keys, approval details and artifact bytes.")
    print(f"Replay cursor {cut} -> {end}: {len(replay['events'])} new events.")
    print("The pending task produced one artifact, which remained intact after restart.")
    print("SQLite integrity_check: ok")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("old_server", type=Path, help="app-server built from tag v0.0.2")
    parser.add_argument("current_server", type=Path, help="app-server built from the current source")
    args = parser.parse_args()
    old = args.old_server.resolve(strict=True)
    current = args.current_server.resolve(strict=True)
    reference = json.loads(subprocess.check_output(
        ["git", "show", "v0.0.2:protocol/fliuno.schema.json"], cwd=ROOT, timeout=30,
    ))
    exported = json.loads(subprocess.check_output([old, "--schema"], cwd=ROOT, timeout=30))
    assert exported == reference, "The old executable must export the v0.0.2 tag schema"

    with TemporaryDirectory(prefix="fliuno-v002-upgrade-") as directory:
        check_upgrade(old, current, Path(directory))


if __name__ == "__main__":
    main()
