#!/usr/bin/env python3
"""Run Linux cgroup kill and resource-limit probes in a delegated subtree."""
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import uuid


def decode_mount_path(value: str) -> str:
    return re.sub(r"\\([0-7]{3})", lambda match: chr(int(match.group(1), 8)), value)


def current_cgroup_directory() -> Path:
    membership = next(
        (
            line[3:]
            for line in Path("/proc/self/cgroup").read_text().splitlines()
            if line.startswith("0::")
        ),
        None,
    )
    if membership is None:
        raise RuntimeError("unified cgroup v2 membership is unavailable")
    cgroup_path = PurePosixPath(membership)
    if not cgroup_path.is_absolute() or ".." in cgroup_path.parts:
        raise RuntimeError("unsafe current cgroup path")

    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        left, separator, right = line.partition(" - ")
        if not separator:
            continue
        mount_fields = left.split()
        filesystem_fields = right.split()
        if len(mount_fields) < 5 or not filesystem_fields or filesystem_fields[0] != "cgroup2":
            continue

        root = PurePosixPath(decode_mount_path(mount_fields[3]))
        mountpoint = Path(decode_mount_path(mount_fields[4]))
        try:
            relative = cgroup_path.relative_to(root)
        except ValueError:
            continue
        current = mountpoint.joinpath(*relative.parts)
        if current.is_dir():
            return current
    raise RuntimeError("current cgroup is not visible in a unified cgroup v2 mount")


def delegated_base() -> Path:
    required = {"cpu", "memory", "pids"}
    current = current_cgroup_directory()
    for candidate in (current, *current.parents):
        control = candidate / "cgroup.subtree_control"
        if control.is_file() and required <= set(control.read_text().split()):
            return candidate
    raise RuntimeError("no ancestor delegates cpu, memory and pids controllers")


def sudo(*arguments: str, input_bytes: bytes | None = None, check: bool = True) -> int:
    result = subprocess.run(
        ["sudo", *arguments],
        input=input_bytes,
        stdout=subprocess.DEVNULL,
        check=False,
    )
    if check and result.returncode:
        raise subprocess.CalledProcessError(result.returncode, ["sudo", *arguments])
    return result.returncode


def cargo_probes(arguments: list[str], environment: dict[str, str], minimum: int = 1) -> int:
    """Stream output and reject a green cargo invocation selecting no probes."""
    passed = 0
    with subprocess.Popen(
        ["cargo", "test", "--locked", *arguments],
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ) as child:
        assert child.stdout is not None
        for line in child.stdout:
            print(line, end="", flush=True)
            match = re.match(r"test result: ok\. (\d+) passed;", line)
            if match:
                passed += int(match[1])
        result = child.wait()
    if result:
        return result
    if passed < minimum:
        raise RuntimeError(f"cgroup probe selector ran {passed} tests; expected at least {minimum}")
    return 0


def main() -> int:
    if os.geteuid() == 0:
        print("Run this check as an unprivileged CI user with sudo access.", file=sys.stderr)
        return 2

    parent = delegated_base() / f"fliuno-ci-{uuid.uuid4()}"
    created = False
    cleanup_errors: list[str] = []
    try:
        sudo("mkdir", str(parent))
        created = True
        owner = f"{os.getuid()}:{os.getgid()}"
        sudo(
            "chown",
            owner,
            str(parent),
            str(parent / "cgroup.procs"),
            str(parent / "cgroup.subtree_control"),
        )
        sudo(
            "tee",
            str(parent / "cgroup.subtree_control"),
            input_bytes=b"+cpu +memory +pids\n",
        )

        environment = os.environ.copy()
        environment["FLIUNO_REQUIRE_DELEGATED_CGROUP"] = "1"
        environment["FLIUNO_REQUIRE_RESOURCE_LIMITS"] = "1"
        environment["FLIUNO_TEST_CGROUP_V2_PARENT"] = str(parent)
        for test in [
            "linux::delegated_cgroup_kills_a_descendant_after_setsid",
            "linux::delegated_cgroup_enforces_the_configured_process_limit",
        ]:
            result = cargo_probes(
                [
                    "-p",
                    "fliuno-sandbox",
                    "--test",
                    "isolation",
                    "--",
                    test,
                    "--exact",
                    "--nocapture",
                ],
                environment,
            )
            if result:
                return result

        guardian_result = cargo_probes(
            [
                "-p", "fliuno-sandbox", "--test", "guardian", "--", "--ignored",
                "--nocapture", "--test-threads=1",
            ],
            environment,
        )
        if guardian_result:
            return guardian_result
        terminal_result = cargo_probes(
            [
                "-p",
                "fliuno-app-server",
                "--all-features",
                "--test",
                "process",
                "--",
                "terminal::linux::",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ],
            environment,
            minimum=2,
        )
        if terminal_result:
            return terminal_result
        return 0
    finally:
        if created:
            if sudo("rmdir", str(parent), check=False):
                cleanup_errors.append("temporary cgroup delegation still has child groups")
        if cleanup_errors:
            raise RuntimeError("; ".join(cleanup_errors))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Linux cgroup integration setup failed: {error}", file=sys.stderr)
        raise SystemExit(2)
