#!/usr/bin/env python3

from __future__ import annotations

import hashlib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
VENDOR_ROOT = REPO_ROOT / "vendor" / "opencypher-tck"
MANIFEST = VENDOR_ROOT / "MANIFEST.sha256"
REVISION = VENDOR_ROOT / "REVISION"
EXPECTED_REVISION = {
    "source": "https://github.com/opencypher/openCypher.git",
    "tag": "2024.3",
    "commit": "677cbafabb8c3c5eed458fd3b1ec0daec8d67d23",
    "content": "tck/features,tck/graphs",
    "license": "Apache-2.0",
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def upstream_paths() -> list[Path]:
    paths = [VENDOR_ROOT / "LICENSE", VENDOR_ROOT / "NOTICE"]
    for directory in (VENDOR_ROOT / "features", VENDOR_ROOT / "graphs"):
        paths.extend(path for path in directory.rglob("*") if path.is_file())
    return sorted(paths)


def load_manifest() -> dict[str, str]:
    entries: dict[str, str] = {}
    for line_number, raw_line in enumerate(MANIFEST.read_text().splitlines(), start=1):
        if not raw_line:
            continue
        try:
            digest, relative_path = raw_line.split("  ", maxsplit=1)
        except ValueError as error:
            raise SystemExit(f"{MANIFEST}:{line_number}: invalid manifest line") from error
        if len(digest) != 64 or any(ch not in "0123456789abcdef" for ch in digest):
            raise SystemExit(f"{MANIFEST}:{line_number}: invalid SHA-256 digest")
        if relative_path in entries:
            raise SystemExit(f"{MANIFEST}:{line_number}: duplicate path {relative_path}")
        entries[relative_path] = digest
    return entries


def check_revision() -> None:
    actual: dict[str, str] = {}
    for line_number, raw_line in enumerate(REVISION.read_text().splitlines(), start=1):
        if not raw_line:
            continue
        if "=" not in raw_line:
            raise SystemExit(f"{REVISION}:{line_number}: invalid revision line")
        key, value = raw_line.split("=", maxsplit=1)
        if not key or key in actual:
            raise SystemExit(f"{REVISION}:{line_number}: invalid or duplicate key {key!r}")
        actual[key] = value

    if actual != EXPECTED_REVISION:
        print("openCypher revision metadata mismatch:")
        for key in sorted(set(actual) | set(EXPECTED_REVISION)):
            expected = EXPECTED_REVISION.get(key, "<missing>")
            observed = actual.get(key, "<missing>")
            if expected != observed:
                print(f"  {key}: expected {expected!r}, got {observed!r}")
        raise SystemExit(1)


def main() -> None:
    check_revision()
    expected = load_manifest()
    paths = upstream_paths()
    actual_names = {path.relative_to(VENDOR_ROOT).as_posix() for path in paths}
    expected_names = set(expected)

    missing = sorted(expected_names - actual_names)
    extra = sorted(actual_names - expected_names)
    if missing or extra:
        if missing:
            print("Missing vendored upstream files:")
            for name in missing:
                print(f"  {name}")
        if extra:
            print("Unexpected vendored upstream files:")
            for name in extra:
                print(f"  {name}")
        raise SystemExit(1)

    failures = []
    for path in paths:
        name = path.relative_to(VENDOR_ROOT).as_posix()
        actual = sha256(path)
        if actual != expected[name]:
            failures.append((name, expected[name], actual))

    if failures:
        print("Vendored upstream checksum mismatches:")
        for name, expected_digest, actual_digest in failures:
            print(f"  {name}")
            print(f"    expected {expected_digest}")
            print(f"    actual   {actual_digest}")
        raise SystemExit(1)

    print(
        "openCypher vendor integrity: "
        f"revision {EXPECTED_REVISION['tag']} / {EXPECTED_REVISION['commit']} and "
        f"{len(paths)} files verified"
    )


if __name__ == "__main__":
    main()
