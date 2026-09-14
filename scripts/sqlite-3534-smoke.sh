#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/sqlite-3534-smoke.sh <extension-path>" >&2
  exit 2
fi

extension=$1
root="${CARGO_TARGET_DIR:-target}/phase10/sqlite-3.53.4"
archive="$root/sqlite-autoconf-3530400.tar.gz"
source_dir="$root/sqlite-autoconf-3530400"
sqlite_bin="$root/sqlite3"
expected_sha256="0e9483900e92cd5de8fd48d16bf9200145a61f7fd5be542a5ac81d8a9516eb9c"

mkdir -p "$root"
if [ ! -f "$archive" ]; then
  curl -fsSL \
    https://www.sqlite.org/2026/sqlite-autoconf-3530400.tar.gz \
    -o "$archive"
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual_sha256=$(sha256sum "$archive" | awk '{print $1}')
else
  actual_sha256=$(shasum -a 256 "$archive" | awk '{print $1}')
fi
if [ "$actual_sha256" != "$expected_sha256" ]; then
  echo "SQLite 3.53.4 fixture checksum mismatch" >&2
  exit 1
fi

if [ ! -d "$source_dir" ]; then
  tar -xzf "$archive" -C "$root"
fi

dynamic_loader_lib=""
if [ "$(uname -s)" = "Linux" ]; then
  dynamic_loader_lib="-ldl"
fi

${CC:-cc} \
  -O2 \
  -DSQLITE_THREADSAFE=1 \
  -DSQLITE_ENABLE_FTS5 \
  "$source_dir/sqlite3.c" \
  "$source_dir/shell.c" \
  -lpthread -lm $dynamic_loader_lib \
  -o "$sqlite_bin"

version=$($sqlite_bin :memory: 'SELECT sqlite_version();')
if [ "$version" != "3.53.4" ]; then
  echo "expected SQLite 3.53.4 fixture, got $version" >&2
  exit 1
fi

LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-sqlite-probe -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase01 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase04 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase05 -- "$extension"
