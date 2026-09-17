#!/usr/bin/env sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: scripts/native-abi-smoke.sh <extension-path> [phase12-tokenizer-extension-path]" >&2
  exit 2
fi

extension=$1
phase12_tokenizer=${2:-}
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_source_dir=${LITHOGRAPH_SQLITE_SOURCE_DIR:-}
output="${CARGO_TARGET_DIR:-target}/phase01/native-abi-smoke"
mkdir -p "$(dirname -- "$output")"

dynamic_loader_lib=""
thread_lib=""
case "$(uname -s)" in
  Darwin)
    thread_lib="-pthread"
    ;;
  Linux)
    dynamic_loader_lib="-ldl"
    thread_lib="-pthread"
    ;;
esac

if [ -n "$sqlite_source_dir" ]; then
  if [ ! -f "$sqlite_source_dir/sqlite3.c" ] || [ ! -f "$sqlite_source_dir/sqlite3.h" ]; then
    echo "SQLite amalgamation not found in $sqlite_source_dir" >&2
    exit 1
  fi
  ${CC:-cc} \
    -std=c11 \
    -Wall -Wextra -Werror \
    -DSQLITE_THREADSAFE=1 \
    -DSQLITE_ENABLE_FTS5 \
    -Iinclude \
    -I"$sqlite_source_dir" \
    tests/native_abi_smoke.c \
    "$sqlite_source_dir/sqlite3.c" \
    $dynamic_loader_lib $thread_lib -lm \
    -o "$output"
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  ${CC:-cc} \
    -std=c11 \
    -Wall -Wextra -Werror \
    -Iinclude \
    -I"$sqlite_prefix/include" \
    -L"$sqlite_prefix/lib" \
    tests/native_abi_smoke.c \
    -lsqlite3 $dynamic_loader_lib $thread_lib \
    -o "$output"
fi

if [ -n "$phase12_tokenizer" ]; then
  "$output" "$extension" "$phase12_tokenizer"
else
  "$output" "$extension"
fi
