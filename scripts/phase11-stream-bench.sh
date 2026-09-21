#!/usr/bin/env sh
set -eu

if [ "$#" -ne 3 ] && [ "$#" -ne 4 ]; then
  echo "usage: scripts/phase11-stream-bench.sh <extension-path> <database-path> <samples> [mode]" >&2
  exit 2
fi

extension=$1
database=$2
samples=$3
mode=${4:-all}
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase11/stream-bench"
mkdir -p "$(dirname -- "$output")"

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/phase11_stream_bench.c \
  -lsqlite3 \
  -o "$output"

"$output" "$extension" "$database" "$samples" "$mode"
