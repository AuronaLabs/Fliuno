"""Package the two native Fliuno workbench distributions from Cargo outputs."""
import argparse
import hashlib
from pathlib import Path
import re
import tarfile
import zipfile


ROOT = Path(__file__).resolve().parents[1]
DOCS = {
    "README.md": ROOT / "README.md",
    "CHANGELOG.md": ROOT / "CHANGELOG.md",
    **{path.relative_to(ROOT).as_posix(): path for path in sorted((ROOT / "docs").rglob("*")) if path.is_file()},
}


def paths(bin_dir, suffix, magic):
    result = {}
    for name in ("fliuno", "fliuno-app-server"):
        name += suffix
        path = bin_dir / name
        with path.open("rb") as source:
            if source.read(len(magic)) != magic:
                raise ValueError(f"Unexpected executable format: {path}")
        result[name] = path
    return {**result, **DOCS}


def package(version, windows, linux, destination, native_packages=()):
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("Version must use MAJOR.MINOR.PATCH")
    destination.mkdir(parents=True, exist_ok=True)
    prefix = f"fliuno-v{version}"
    win_name, linux_name = f"{prefix}-windows-x64", f"{prefix}-linux-x64"
    assets = []
    if windows is not None:
        win_archive = destination / f"{win_name}.zip"
        with zipfile.ZipFile(win_archive, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, path in paths(windows, ".exe", b"MZ").items():
                archive.write(path, f"{win_name}/{name}")
        assets.append(win_archive)
    if linux is not None:
        linux_archive = destination / f"{linux_name}.tar.gz"
        with tarfile.open(linux_archive, "w:gz") as archive:
            for name, path in paths(linux, "", b"\x7fELF").items():
                info = archive.gettarinfo(str(path), f"{linux_name}/{name}")
                info.mode = 0o644 if name in DOCS else 0o755
                info.uid = info.gid = 0
                info.uname = info.gname = "root"
                with path.open("rb") as source:
                    archive.addfile(info, source)
        assets.append(linux_archive)
    assets.extend(native_packages)
    lines = []
    for path in sorted(assets, key=lambda asset: asset.name):
        with path.open("rb") as source:
            lines.append(f"{hashlib.file_digest(source, 'sha256').hexdigest()}  {path.name}\n")
    print("".join(lines), end="")
    (destination / "SHA256SUMS.txt").write_text("".join(lines), encoding="ascii", newline="\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("--windows-bin", type=Path)
    parser.add_argument("--linux-bin", type=Path)
    parser.add_argument("--native-package", type=Path, action="append", default=[])
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if args.windows_bin is None and args.linux_bin is None:
        parser.error("supply --windows-bin, --linux-bin, or both")
    package(args.version, args.windows_bin, args.linux_bin, args.out, args.native_package)
