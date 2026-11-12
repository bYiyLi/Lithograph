#!/usr/bin/env sh
set -eu
if [ "$#" -ne 2 ]; then
  echo "usage: sh scripts/phase22-noindex-read.sh <extension> <synthetic-database>" >&2
  exit 2
fi
sqlite_bin=$(command -v "${LITHOGRAPH_SQLITE3:-sqlite3}")
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase22-s3/noindex-read-work"
mkdir -p "$(dirname -- "$output")"
loader_lib=""
if [ "$(uname -s)" = "Linux" ]; then loader_lib="-ldl"; fi
${CC:-cc} -std=c11 -Wall -Wextra -Werror -O2 \
  -I"$sqlite_prefix/include" -L"$sqlite_prefix/lib" \
  -Wl,-rpath,"$sqlite_prefix/lib" \
  tests/phase22_noindex_read_work.c -lsqlite3 $loader_lib -o "$output"
"$output" "$1" "$2"
