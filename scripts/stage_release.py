"""Verify paired installer artifacts and stage their release files locally."""
import argparse
from hashlib import file_digest
from pathlib import Path
import re
import shutil
import stat
import tarfile
import zipfile


def read_manifest(path):
    entries = {}
    for line in path.read_text(encoding="ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        if not match or match[2] in entries:
            raise ValueError(f"Invalid or repeated checksum entry in {path}")
        entries[match[2]] = match[1]
    return entries


def check_archive(asset):
    if asset.suffix == ".zip":
        with zipfile.ZipFile(asset) as archive:
            if archive.testzip() is not None:
                raise ValueError(f"Invalid ZIP data in {asset}")
            for executable in ("fliuno.exe", "fliuno-app-server.exe"):
                with archive.open(f"{asset.stem}/{executable}") as entry:
                    if entry.read(2) != b"MZ":
                        raise ValueError(f"Invalid Windows binary: {asset}/{executable}")
    elif asset.name.endswith(".tar.gz"):
        prefix = asset.name.removesuffix(".tar.gz")
        with tarfile.open(asset, "r:gz") as archive:
            for executable in ("fliuno", "fliuno-app-server"):
                member = archive.getmember(f"{prefix}/{executable}")
                if not (member.isfile() and member.mode & stat.S_IXUSR):
                    raise ValueError(f"Not an executable file: {asset}/{executable}")
                with archive.extractfile(member) as entry:
                    if entry.read(4) != b"\x7fELF":
                        raise ValueError(f"Invalid Linux binary: {asset}/{executable}")


def digest(path):
    with path.open("rb") as stream:
        return file_digest(stream, "sha256").hexdigest()


def stage(artifacts, output, version, source_commit):
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("Version must use MAJOR.MINOR.PATCH")
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        raise ValueError("Expected the full lowercase Git commit SHA")
    if output.exists():
        raise ValueError(f"Output already exists: {output}")
    prefix = f"fliuno-v{version}"
    groups = {
        "fliuno-windows-candidate": {f"{prefix}-windows-x64.msi", f"{prefix}-windows-x64.zip"},
        "fliuno-debian-candidate": {f"{prefix}-linux-amd64.deb", f"{prefix}-linux-x64.tar.gz"},
    }
    manifest = {}
    for group, names in groups.items():
        directory = artifacts / group
        reported_commit = (directory / "SOURCE-COMMIT.txt").read_text(encoding="utf-8-sig").strip()
        if reported_commit != source_commit:
            raise ValueError(f"Wrong source commit for {group}: {reported_commit}")
        checksums = read_manifest(directory / "SHA256SUMS.txt")
        if checksums.keys() != names:
            raise ValueError(f"Unexpected files in {group} checksum manifest")
        for name, expected in checksums.items():
            path = directory / name
            if digest(path) != expected:
                raise ValueError(f"Checksum mismatch: {path}")
            check_archive(path)
            manifest[name] = (path, expected)

    output.mkdir()
    for name, (path, expected) in sorted(manifest.items()):
        staged = output / name
        shutil.copyfile(path, staged)
        if digest(staged) != expected:
            raise ValueError(f"Checksum mismatch after copying: {staged}")
    (output / "SHA256SUMS.txt").write_text(
        "".join(f"{expected}  {name}\n" for name, (_, expected) in sorted(manifest.items())),
        encoding="ascii", newline="\n",
    )
    print(f"Verified and staged {len(manifest)} release assets from {source_commit}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path)
    parser.add_argument("output", type=Path, help="new output directory")
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    try:
        stage(args.artifacts, args.output, args.version, args.commit)
    except (ValueError, OSError, KeyError, zipfile.BadZipFile, tarfile.TarError) as error:
        parser.exit(1, f"Release staging failed: {error}\n")
