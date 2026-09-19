#!/usr/bin/env python3
"""Build a GitHub Release archive for one Lithograph target."""

from __future__ import annotations

import argparse
import shutil
import tarfile
import tempfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
PACKAGE_FILES = ("README.md", "LICENSE", "COMMERCIAL-LICENSE.md")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    parser.add_argument("--binary", required=True, action="append", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def stage_package(tag: str, binaries: list[Path], directory: Path) -> None:
    if not tag.startswith("v") or len(tag) < 2:
        raise ValueError(f"invalid release tag: {tag}")
    if len({binary.name for binary in binaries}) != len(binaries):
        raise ValueError("release binaries must have distinct file names")
    for binary in binaries:
        if not binary.is_file():
            raise FileNotFoundError(binary)

    for binary in binaries:
        shutil.copy2(binary, directory / binary.name)
    for name in PACKAGE_FILES:
        shutil.copy2(ROOT / name, directory / name)
    (directory / "VERSION").write_text(f"{tag[1:]}\n", encoding="utf-8")


def write_archive(source: Path, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    members = sorted(source.iterdir(), key=lambda path: path.name)
    if output.name.endswith(".tar.gz"):
        with tarfile.open(output, "w:gz") as archive:
            for member in members:
                archive.add(member, arcname=member.name)
        return
    if output.suffix == ".zip":
        with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for member in members:
                archive.write(member, arcname=member.name)
        return
    raise ValueError(f"unsupported release archive: {output}")


def main() -> None:
    args = parse_args()
    binaries = [binary.resolve() for binary in args.binary]
    output = args.output.resolve()
    with tempfile.TemporaryDirectory(prefix="lithograph-release-") as temporary:
        staging = Path(temporary)
        stage_package(args.tag, binaries, staging)
        write_archive(staging, output)
    print(output)


if __name__ == "__main__":
    main()
