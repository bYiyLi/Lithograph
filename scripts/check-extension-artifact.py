#!/usr/bin/env python3
from __future__ import annotations

import platform
import subprocess
import sys
from pathlib import Path


REQUIRED_SYMBOLS = {
    "sqlite3_lithograph_init",
    "lithograph_v1_execute",
    "lithograph_v1_validate",
    "lithograph_v1_tx_begin",
    "lithograph_v1_tx_execute",
    "lithograph_v1_tx_commit",
    "lithograph_v1_tx_abort",
    "lithograph_v1_free",
}
PROVIDER_REQUIRED_SYMBOLS = {
    "sqlite3_extension_init",
    "sqlite3_lithographopenaicompatible_init",
}


def run(*args: str) -> str:
    completed = subprocess.run(args, check=True, text=True, capture_output=True)
    return completed.stdout


def main() -> int:
    if len(sys.argv) not in (2, 3) or (len(sys.argv) == 3 and sys.argv[2] != "--provider"):
        print(
            "usage: scripts/check-extension-artifact.py <extension-path> [--provider]",
            file=sys.stderr,
        )
        return 2

    artifact = Path(sys.argv[1]).resolve()
    required_symbols = PROVIDER_REQUIRED_SYMBOLS if len(sys.argv) == 3 else REQUIRED_SYMBOLS
    if not artifact.is_file():
        print(f"extension artifact does not exist: {artifact}", file=sys.stderr)
        return 1

    system = platform.system()
    if system == "Darwin":
        dependencies = run("otool", "-L", str(artifact))
        symbols = run("nm", "-gU", str(artifact))
        undefined = run("nm", "-u", str(artifact))
        dependency_lines = dependencies.lower().splitlines()[1:]
        exported = {
            line.rsplit(maxsplit=1)[-1].removeprefix("_")
            for line in symbols.splitlines()
            if line.strip()
        }
    elif system == "Linux":
        dependencies = run("ldd", str(artifact))
        symbols = run("nm", "-D", "--defined-only", str(artifact))
        undefined = run("nm", "-D", "--undefined-only", str(artifact))
        dependency_lines = dependencies.lower().splitlines()
        exported = {
            line.rsplit(maxsplit=1)[-1]
            for line in symbols.splitlines()
            if line.strip()
        }
    else:
        print(f"unsupported artifact-inspection host: {system}", file=sys.stderr)
        return 2

    if any("sqlite" in line for line in dependency_lines):
        print("extension artifact must not link a private SQLite runtime", file=sys.stderr)
        print(dependencies, file=sys.stderr)
        return 1

    sqlite_imports = [
        line for line in undefined.splitlines() if "sqlite3_" in line.lower()
    ]
    if sqlite_imports:
        print(
            "extension artifact has direct SQLite symbol imports instead of using the host API table",
            file=sys.stderr,
        )
        print("\n".join(sqlite_imports), file=sys.stderr)
        return 1

    missing = sorted(required_symbols - exported)
    if missing:
        print(f"missing required exported symbols: {', '.join(missing)}", file=sys.stderr)
        return 1

    print(f"artifact inspection passed: {artifact}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
