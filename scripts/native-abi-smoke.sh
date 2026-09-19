#!/usr/bin/env sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 3 ]; then
  echo "usage: scripts/native-abi-smoke.sh <extension-path> [phase12-tokenizer-extension-path] [phase13-synthetic-provider-path]" >&2
  exit 2
fi

extension=$1
phase12_tokenizer=${2:-}
phase13_provider=${3:-}
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
  smoke_object="${output}.o"
  sqlite_object="${output}.sqlite3.o"
  ${CC:-cc} \
    -std=c11 \
    -Wall -Wextra -Werror \
    -DSQLITE_THREADSAFE=1 \
    -DSQLITE_ENABLE_FTS5 \
    -Iinclude \
    -I"$sqlite_source_dir" \
    tests/native_abi_smoke.c \
    -c \
    -o "$smoke_object"
  ${CC:-cc} \
    -std=c11 \
    -w \
    -DSQLITE_THREADSAFE=1 \
    -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_source_dir" \
    "$sqlite_source_dir/sqlite3.c" \
    -c \
    -o "$sqlite_object"
  ${CC:-cc} \
    "$smoke_object" \
    "$sqlite_object" \
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

if [ -n "$phase13_provider" ]; then
  "$output" "$extension" "$phase12_tokenizer" "$phase13_provider"
elif [ -n "$phase12_tokenizer" ]; then
  "$output" "$extension" "$phase12_tokenizer"
else
  "$output" "$extension"
fi
