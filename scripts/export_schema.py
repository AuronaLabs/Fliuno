"""Export protocol contracts from Rust; --check detects committed schema drift."""
import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / "protocol" / "fliuno.schema.json"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--app-server", type=Path, help="Use an already built app-server")
    args = parser.parse_args()
    command = ([str(args.app_server.resolve(strict=True)), "--schema"] if args.app_server else
               ["cargo", "run", "--quiet", "--locked", "-p", "fliuno-app-server", "--", "--schema"])
    output = subprocess.run(
        command,
        cwd=ROOT, capture_output=True, check=True,
    )
    schema = json.loads(output.stdout)
    if args.check:
        if not TARGET.exists() or json.loads(TARGET.read_text(encoding="utf-8")) != schema:
            raise SystemExit("Schema drift: run python scripts/export_schema.py")
        print("Protocol schema matches Rust types.")
    else:
        TARGET.parent.mkdir(parents=True, exist_ok=True)
        TARGET.write_text(json.dumps(schema, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(TARGET)


if __name__ == "__main__":
    main()
