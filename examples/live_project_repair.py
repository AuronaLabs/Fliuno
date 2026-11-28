"""Opt-in live model coding evaluation using an isolated temporary Git project."""
import argparse
import hashlib
import json
import os
from pathlib import Path
from queue import Empty, Queue
import subprocess
import tempfile
from threading import Thread
import time
import tomllib


SOURCE = "src/order-total.cjs"
BEFORE = """'use strict';

function orderTotal(items) {
  return items.reduce((total, item) => total + item.unitCents, 0);
}

module.exports = { orderTotal };
"""
TEST = """'use strict';
const assert = require('node:assert/strict');
const { orderTotal } = require('./src/order-total.cjs');
assert.equal(orderTotal([]), 0, 'an empty order costs zero');
assert.equal(orderTotal([{unitCents: 210, quantity: 1}]), 210, 'one item');
assert.equal(orderTotal([{unitCents: 175, quantity: 2}, {unitCents: 50, quantity: 3}]),
  500, 'order total must include quantities on every line');
assert.equal(orderTotal([{unitCents: 999, quantity: 0}]), 0, 'zero quantity');
console.log('4 order-total assertions passed');
"""
PROMPT = """购物车多件同商品时，订单总价偏低。请在当前项目定位原因并修复。
先运行已配置的 Order total unit tests 取得失败证据；检查所需哈希可通过读取工具获取。
只修改 src/order-total.cjs 的实现，不改测试。然后重跑同一个检查，报告实际结果。
为了把调用留给定位、修改和测试，这次不需要发布计划；如果检查没跑通，请如实说明。"""


def run(args, directory, evidence):
    project = directory / "order-project"
    (project / "src").mkdir(parents=True)
    (project / SOURCE).write_bytes(BEFORE.encode())
    test = directory / "order.test.cjs"
    test.write_bytes(TEST.encode())
    asset = {"source": str(test), "path": test.name,
             "sha256": hashlib.sha256(test.read_bytes()).hexdigest()}
    node_args = ["--preserve-symlinks", "--preserve-symlinks-main"]
    if os.name != "nt":
        config = directory / "openssl.cnf"
        config.write_text("# Use Node's default OpenSSL provider.\n", encoding="utf-8")
        node_args.append("--openssl-config=openssl.cnf")
        assets = [asset, {"source": str(config), "path": config.name,
                          "sha256": hashlib.sha256(config.read_bytes()).hexdigest()}]
    else:
        assets = [asset]

    def git(*command):
        return subprocess.check_output(["git", "-c", "core.autocrlf=false", *command],
                                       cwd=project, encoding="utf-8")

    git("init", "-q")
    git("add", "--", SOURCE)
    checks = directory / "checks.json"
    checks.write_text(json.dumps({"programs": [{
        "id": "order_total",
        "command": str(args.node.resolve(strict=True)),
        "isolation": "windows_app_container" if os.name == "nt" else "linux_landlock",
        "args": [*node_args, test.name], "assets": assets, "timeoutMs": 20000,
        "projectCheck": {"label": "Order total unit tests",
                         "description": "Run pinned regression assertions for order totals.",
                         "inputs": [SOURCE]},
    }]}), encoding="utf-8")
    command = [str(args.app_server.resolve(strict=True)), "--config", str(args.config),
               "--db", str(directory / "kernel.db"), "--exec-config", str(checks)]
    if args.model_profile:
        command.extend(["--model-profile", args.model_profile])
    with subprocess.Popen(command, cwd=directory, text=True, encoding="utf-8",
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL) as server:
        incoming = Queue()

        def receive():
            try:
                for line in server.stdout:
                    incoming.put(json.loads(line))
            finally:
                incoming.put(None)

        Thread(target=receive, daemon=True).start()
        request_id = 0

        def call(method, params=None):
            nonlocal request_id
            request_id += 1
            data = {"jsonrpc": "2.0", "id": request_id, "method": method}
            if params is not None:
                data["params"] = params
            server.stdin.write(json.dumps(data) + "\n")
            server.stdin.flush()
            deadline = time.monotonic() + 20
            while True:
                try:
                    reply = incoming.get(timeout=max(0, deadline - time.monotonic()))
                except Empty as error:
                    raise TimeoutError(f"{method} did not respond") from error
                if reply is None:
                    raise RuntimeError("app-server closed its output")
                if "method" in reply and "id" not in reply:
                    continue
                if reply.get("id") != request_id or "error" in reply:
                    raise RuntimeError(f"{method} failed: {reply.get('error', {}).get('code')}")
                return reply["result"]

        try:
            hello = call("initialize", {"protocolVersion": "0.1", "clientName": "live-project-repair"})
            info = hello["capabilities"].get("modelCapabilities", {})
            evidence["model"] = info.get("model")
            tools = call("tools/list")
            check_name, = [tool["name"] for tool in tools if tool["name"].startswith("process_order_total_")]
            workspace = call("workspace/open", {"root": str(project)})
            session = call("session/create", {"workspaceId": workspace["id"]})
            task = call("task/start", {"sessionId": session["id"], "input": PROMPT,
                                       "maxModelSteps": args.steps})
            task_id = task["id"]
            deadline = time.monotonic() + args.timeout
            approved = set()
            while task["status"] not in {"completed", "cancelled", "failed", "interrupted"}:
                if time.monotonic() >= deadline:
                    call("task/cancel", {"taskId": task_id})
                    raise TimeoutError("repair exceeded its wall-clock budget")
                if pending := task.get("pendingApproval"):
                    if pending["id"] in approved:
                        raise RuntimeError("approval did not advance")
                    tool = pending["call"]
                    name, arguments = tool["name"], tool["arguments"]
                    path = arguments.get("path")
                    allow = (
                        (name in {"workspace_find", "workspace_list"} and path in {".", "src"})
                        or (name in {"workspace_read_text", "workspace_sha256"} and path == SOURCE)
                        or (name == "workspace_sha256_batch" and arguments.get("paths") == [SOURCE])
                        or (name in {"workspace_patch_text", "workspace_replace_text", "workspace_edit_text"} and path == SOURCE)
                        or (name == check_name and set(arguments.get("inputs", {})) == {SOURCE})
                    )
                    evidence["approvals"].append({"step": task["modelSteps"], "tool": name,
                                                  "path": path, "allowed": allow})
                    print(f"step {task['modelSteps']}: {name} {'approved' if allow else 'denied'}", flush=True)
                    approved.add(pending["id"])
                    call("approval/respond", {"taskId": task_id, "approvalId": pending["id"],
                                              "decision": "allow_once" if allow else "deny"})
                elif task.get("pendingToolInput"):
                    call("task/cancel", {"taskId": task_id})
                    raise RuntimeError("the model asked for input in a self-contained evaluation")
                time.sleep(0.16)
                task = call("task/get", {"taskId": task_id})
            evidence["status"] = task["status"]
            evidence["attempts"] = task["modelSteps"]
            evidence["answer"] = task.get("output")
            evidence["patches"] = [
                call["arguments"]["patch"]
                for message in task["messages"] if (call := message.get("call"))
                and call.get("name") == "workspace_patch_text"
                and call.get("arguments", {}).get("path") == SOURCE
            ]
            for message in task["messages"]:
                result = message.get("output", {})
                if isinstance(result, dict) and result.get("status") == "not_applied":
                    evidence["corrections"].append({
                        "callId": message.get("callId"), "code": result.get("code"),
                    })
                if isinstance(result, dict) and result.get("projectCheck", {}).get("kind") == "project_check":
                    evidence["checks"].append({
                        "exitCode": result.get("exitCode"), "termination": result.get("termination"),
                        "success": result.get("success"), "stdout": result.get("stdout", {}).get("data"),
                        "stderr": result.get("stderr", {}).get("data"),
                        "inputs": result["projectCheck"].get("inputs"),
                    })
            evidence["gitStatus"] = git("status", "--short", "--untracked-files=all").splitlines()
            evidence["diff"] = git("diff", "--no-ext-diff", "--", SOURCE)
            evidence["baselineSha256"] = hashlib.sha256(BEFORE.encode()).hexdigest()
            evidence["finalSha256"] = hashlib.sha256((project / SOURCE).read_bytes()).hexdigest()
            recorded = evidence["checks"]
            evidence["passed"] = bool(
                evidence["status"] == "completed" and evidence["gitStatus"] == [f"AM {SOURCE}"]
                and len(recorded) >= 2 and recorded[0]["exitCode"] == 1 and not recorded[0]["success"]
                and "order total must include quantities" in (recorded[0]["stderr"] or "")
                and recorded[0]["inputs"] == {SOURCE: evidence["baselineSha256"]}
                and recorded[-1]["exitCode"] == 0 and recorded[-1]["success"]
                and recorded[-1]["inputs"] == {SOURCE: evidence["finalSha256"]}
                and "4 order-total assertions passed" in (recorded[-1]["stdout"] or "")
            )
        finally:
            server.stdin.close()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path, help="model-only Fliuno TOML configuration")
    parser.add_argument("--model-profile")
    parser.add_argument("--app-server", required=True, type=Path)
    parser.add_argument("--node", required=True, type=Path)
    parser.add_argument("--scratch", required=True, type=Path, help="existing directory for temporary files")
    parser.add_argument("--evidence", required=True, type=Path, help="new report file")
    parser.add_argument("--steps", type=int, choices=range(1, 33), default=8)
    parser.add_argument("--timeout", type=int, default=180)
    args = parser.parse_args()
    args.config = args.config.resolve(strict=True)
    settings = tomllib.loads(args.config.read_text(encoding="utf-8-sig"))
    if settings.get("integrations"):
        parser.error("use a temporary, model-only configuration to avoid loading unrelated integrations")
    evidence = {"passed": False, "approvals": [], "checks": [], "corrections": []}
    with args.evidence.open("x", encoding="utf-8") as report:
        try:
            with tempfile.TemporaryDirectory(prefix="fliuno-live-repair-", dir=args.scratch) as temp:
                run(args, Path(temp).resolve(), evidence)
        finally:
            json.dump(evidence, report, ensure_ascii=False, indent=2)
            report.write("\n")
    summary = {key: evidence.get(key) for key in ("model", "status", "attempts", "passed")}
    summary["checkExits"] = [check["exitCode"] for check in evidence["checks"]]
    summary["corrections"] = [item["code"] for item in evidence["corrections"]]
    print(json.dumps(summary))
    raise SystemExit(0 if evidence["passed"] else 1)


if __name__ == "__main__":
    main()
