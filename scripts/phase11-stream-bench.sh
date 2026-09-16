#!/usr/bin/env sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: scripts/phase11-stream-bench.sh <extension-path> <database-path> <samples>" >&2
  exit 2
fi

extension=$1
database=$2
samples=$3
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase11/stream-bench"
mkdir -p "$(dirname -- "$output")"

dynamic_loader_lib=""
case "$(uname -s)" in
  Darwin) ;;
  Linux) dynamic_loader_lib="-ldl" ;;
esac

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -Iinclude \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/phase11_stream_bench.c \
  -lsqlite3 $dynamic_loader_lib \
  -o "$output"

"$output" "$extension" "$database" "$samples"
