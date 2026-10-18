#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/phase18-external-access-smoke.sh <extension-path>" >&2
  exit 2
fi

extension=$1
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_bin=$(command -v "$sqlite_bin")
sqlite_source_dir=${LITHOGRAPH_SQLITE_SOURCE_DIR:-}
target_dir=${CARGO_TARGET_DIR:-target}
output_dir="$target_dir/phase18/external-access"
output="$output_dir/smoke"
mkdir -p "$output_dir"

case "$(uname -s)" in
  Darwin)
    provider="$output_dir/synthetic-provider.dylib"
    tokenizer="$output_dir/phase12-tokenizer.dylib"
    dynamic_loader_lib=""
    ;;
  Linux)
    provider="$output_dir/synthetic-provider.so"
    tokenizer="$output_dir/phase12-tokenizer.so"
    dynamic_loader_lib="-ldl"
    ;;
  *)
    echo "Phase 18 external-access smoke is built by Unix gates" >&2
    exit 2
    ;;
esac

if [ -n "$sqlite_source_dir" ]; then
  if [ ! -f "$sqlite_source_dir/sqlite3.c" ] || [ ! -f "$sqlite_source_dir/sqlite3ext.h" ]; then
    echo "SQLite amalgamation not found in $sqlite_source_dir" >&2
    exit 1
  fi
  sqlite_include=$sqlite_source_dir
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  sqlite_include="$sqlite_prefix/include"
fi

scripts/build-phase13-synthetic-provider.sh "$sqlite_include" "$provider"
scripts/build-phase12-tokenizer.sh "$sqlite_include" "$tokenizer"

if [ -n "$sqlite_source_dir" ]; then
  ${CC:-cc} -std=c11 -Wall -Wextra -Werror -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_include" -c tests/phase18_external_access_smoke.c -o "$output.o"
  ${CC:-cc} -std=c11 -w -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_include" -c "$sqlite_source_dir/sqlite3.c" -o "$output.sqlite3.o"
  ${CC:-cc} "$output.o" "$output.sqlite3.o" $dynamic_loader_lib -pthread -lm -o "$output"
else
  ${CC:-cc} -std=c11 -Wall -Wextra -Werror -I"$sqlite_include" \
    -L"$sqlite_prefix/lib" tests/phase18_external_access_smoke.c \
    -lsqlite3 $dynamic_loader_lib -pthread -lm -o "$output"
fi

"$output" "$extension" "$provider" "$tokenizer"
