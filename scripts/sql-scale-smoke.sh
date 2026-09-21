#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: scripts/sql-scale-smoke.sh <extension-path> <database-path>" >&2
  exit 2
fi

extension=$1
database=$2
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase10/sql-scale-smoke"
mkdir -p "$(dirname -- "$output")"

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/sql_scale_smoke.c \
  -lsqlite3 \
  -o "$output"

"$output" "$extension" "$database"
