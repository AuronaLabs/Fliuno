"""Verify an installed binary pair against a profile created by a stable release."""
from argparse import ArgumentParser
from contextlib import contextmanager
from hashlib import sha256
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import time
from urllib.request import urlopen
import zipfile


CREDENTIAL = b"installer-fixture-key\n"
PROFILE = """[workbench]
database = "state/workbench.db"
[model]
provider = "openai"
name = "installer-fixture"
api_key_file = "credentials/openai.key"
base_url = "http://127.0.0.1:1/v1/"
"""
MEMORY = {"key": "installer-check", "content": "Keep project data through package changes."}
PROMPT = "Create an artifact before upgrading the package."


def download(tag, platform, directory):
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Baseline must be a release tag such as v0.0.16")
    directory.mkdir(parents=True)
    suffix = ".exe" if platform == "windows" else ""
    extension = ".zip" if platform == "windows" else ".tar.gz"
    prefix = f"fliuno-{tag}-{platform}-x64"
    name = prefix + extension
    base = f"https://github.com/AuronaLabs/Fliuno/releases/download/{tag}"
    with urlopen(f"{base}/SHA256SUMS.txt", timeout=30) as response:
        manifest = dict(line.split()[::-1] for line in response.read().decode("ascii").splitlines())
    bundle = directory / name
    with urlopen(f"{base}/{name}", timeout=60) as response, bundle.open("wb") as output:
        shutil.copyfileobj(response, output)
    assert sha256(bundle.read_bytes()).hexdigest() == manifest[name]
    with (zipfile.ZipFile(bundle) if platform == "windows" else tarfile.open(bundle)) as archive:
        for executable in ("fliuno", "fliuno-app-server"):
            file = executable + suffix
            member = f"{prefix}/{file}"
            source = archive.open(member) if platform == "windows" else archive.extractfile(member)
            with source, (directory / file).open("wb") as output:
                shutil.copyfileobj(source, output)
            (directory / file).chmod(0o755)


@contextmanager
def connect(server, database, version, *, config=None):
    mode = ["--config", str(config)] if config is not None else ["--demo"]
    with subprocess.Popen(
        [str(server), "--db", str(database), *mode],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, encoding="utf-8",
    ) as process:
        counter = 0

        def call(method, params):
            nonlocal counter
            counter += 1
            request = {"jsonrpc": "2.0", "id": counter, "method": method, "params": params}
            process.stdin.write(json.dumps(request) + "\n")
            process.stdin.flush()
            for line in process.stdout:
                reply = json.loads(line)
                if reply.get("id") == counter:
                    if "error" in reply:
                        raise RuntimeError(f"{method}: {reply['error']}")
                    return reply["result"]
            raise RuntimeError(f"App-server closed before replying to {method}")

        try:
            info = call("initialize", {"protocolVersion": "0.1", "clientName": "installer-check"})
            assert info["serverVersion"] == version, info
            yield call
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        assert process.returncode == 0


def wait_for(call, task, expected):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        state = call("task/get", {"taskId": task})
        if state["status"] == expected:
            return state
        time.sleep(0.04)
    raise RuntimeError(f"Task did not reach {expected}: {state}")


def seed(directory, server, version):
    directory.mkdir(parents=True)
    profile = directory / ".fliuno"
    (profile / "credentials").mkdir(parents=True)
    (profile / "state").mkdir()
    (directory / "project").mkdir()
    (profile / "config.toml").write_bytes(PROFILE.encode("ascii"))
    (profile / "credentials/openai.key").write_bytes(CREDENTIAL)
    with connect(server, profile / "state/workbench.db", version) as call:
        workspace = call("workspace/open", {"root": str(directory / "project")})["id"]
        session = call("session/create", {"workspaceId": workspace, "title": "Installer migration"})["id"]
        call("workspace/memory/put", {"workspaceId": workspace, **MEMORY})
        task = call("task/start", {"sessionId": session, "input": PROMPT})["id"]
        approval = wait_for(call, task, "waiting_approval")["pendingApproval"]["id"]
        call("approval/respond", {"taskId": task, "approvalId": approval, "decision": "allow_once"})
        wait_for(call, task, "completed")
        items = call("artifacts/listSummaries", {"taskId": task})["items"]
        assert items, "Stable release did not produce an artifact"
    (directory / "fixture.json").write_text(json.dumps({
        "workspace": workspace, "session": session, "task": task, "artifact": items[0]["id"],
    }), encoding="utf-8")


def verify(directory, server, version):
    profile = directory / ".fliuno"
    config = profile / "config.toml"
    assert config.read_bytes() == PROFILE.encode("ascii")
    assert (profile / "credentials/openai.key").read_bytes() == CREDENTIAL
    database = profile / "state/workbench.db"
    assert database.is_file()
    fixture = json.loads((directory / "fixture.json").read_text(encoding="utf-8"))
    with connect(server, database, version) as call:
        session = call("session/get", {"sessionId": fixture["session"]})
        assert session["workspaceId"] == fixture["workspace"]
        task = call("task/get", {"taskId": fixture["task"]})
        assert task["status"] == "completed" and task["messages"][0]["text"] == PROMPT
        artifacts = call("artifacts/listSummaries", {"taskId": fixture["task"]})["items"]
        assert fixture["artifact"] in {artifact["id"] for artifact in artifacts}
        memory = call("workspace/memory/get", {"workspaceId": fixture["workspace"], "key": MEMORY["key"]})
        assert memory["content"] == MEMORY["content"]
        follow_up = call("task/start", {
            "sessionId": fixture["session"], "input": "Continue after upgrade.", "context": {"planOnly": True},
        })
        wait_for(call, follow_up["id"], "completed")

    tui = server.with_name("fliuno" + server.suffix)
    result = subprocess.run([str(tui), "--version"], capture_output=True, text=True, check=True, timeout=10)
    assert result.stdout.strip() == f"fliuno {version}"
    result = subprocess.run([str(tui), "--doctor", "--config", str(config)],
                            capture_output=True, text=True, check=True, timeout=10)
    assert CREDENTIAL.decode().strip() not in result.stdout + result.stderr
    print(f"Verified Fliuno {version}, the migrated profile, and a follow-up turn")


if __name__ == "__main__":
    parser = ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest="action", required=True)
    fetch = actions.add_parser("download")
    fetch.add_argument("tag")
    fetch.add_argument("platform", choices=("windows", "linux"))
    fetch.add_argument("directory", type=Path)
    for action in ("seed", "verify"):
        mode = actions.add_parser(action)
        mode.add_argument("directory", type=Path)
        mode.add_argument("server", type=Path)
        mode.add_argument("version")
    args = parser.parse_args()
    directory = args.directory.resolve()
    if args.action == "download":
        download(args.tag, args.platform, directory)
    else:
        (seed if args.action == "seed" else verify)(directory, args.server.resolve(), args.version)
