#!/usr/bin/env sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: scripts/phase11-mixed-stress.sh <extension-path> <database-path> <readers> <duration-seconds>" >&2
  exit 2
fi

extension=$1
database=$2
readers=$3
duration=$4
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase11/mixed-stress"
mkdir -p "$(dirname -- "$output")"

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/phase11_mixed_stress.c \
  -lsqlite3 -pthread \
  -o "$output"

"$output" "$extension" "$database" "$readers" "$duration"
