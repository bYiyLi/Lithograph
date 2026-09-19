#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/sql-tx-smoke.sh <extension-path>" >&2
  exit 2
fi

extension=$1
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_source_dir=${LITHOGRAPH_SQLITE_SOURCE_DIR:-}
output="${CARGO_TARGET_DIR:-target}/phase14/sql-tx-smoke"
database="${output}.db"
mkdir -p "$(dirname -- "$output")"

dynamic_loader_lib=""
thread_lib=""
case "$(uname -s)" in
  Darwin) thread_lib="-pthread" ;;
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
    -std=c11 -Wall -Wextra -Werror \
    -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_source_dir" \
    tests/sql_tx_smoke.c -c -o "$smoke_object"
  ${CC:-cc} \
    -std=c11 -w \
    -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_source_dir" \
    "$sqlite_source_dir/sqlite3.c" -c -o "$sqlite_object"
  ${CC:-cc} "$smoke_object" "$sqlite_object" \
    $dynamic_loader_lib $thread_lib -lm -o "$output"
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  ${CC:-cc} \
    -std=c11 -Wall -Wextra -Werror \
    -I"$sqlite_prefix/include" -L"$sqlite_prefix/lib" \
    tests/sql_tx_smoke.c -lsqlite3 $dynamic_loader_lib $thread_lib \
    -o "$output"
fi

"$output" "$extension" "$database"
