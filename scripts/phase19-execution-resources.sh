#!/usr/bin/env sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: scripts/phase19-execution-resources.sh <extension-path> [case]" >&2
  exit 2
fi

extension=$1
selected=${2:-all}
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_source_dir=${LITHOGRAPH_SQLITE_SOURCE_DIR:-}
output="${CARGO_TARGET_DIR:-target}/phase19/execution-resources"
mkdir -p "$(dirname -- "$output")"
loader_lib=""
thread_lib=""
case "$(uname -s)" in
  Darwin) thread_lib="-pthread" ;;
  Linux) loader_lib="-ldl"; thread_lib="-pthread" ;;
esac

if [ -n "$sqlite_source_dir" ]; then
  if [ ! -f "$sqlite_source_dir/sqlite3.c" ] || [ ! -f "$sqlite_source_dir/sqlite3.h" ]; then
    echo "SQLite amalgamation not found in $sqlite_source_dir" >&2
    exit 1
  fi
  ${CC:-cc} -std=c11 -Wall -Wextra -Werror -O2 \
    -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 -I"$sqlite_source_dir" \
    -c tests/phase19_execution_resources.c -o "$output.o"
  ${CC:-cc} -std=c11 -w -O2 -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_source_dir" -c "$sqlite_source_dir/sqlite3.c" -o "$output.sqlite3.o"
  ${CC:-cc} "$output.o" "$output.sqlite3.o" $loader_lib $thread_lib -lm -o "$output"
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  ${CC:-cc} -std=c11 -Wall -Wextra -Werror -O2 \
    -I"$sqlite_prefix/include" -L"$sqlite_prefix/lib" \
    tests/phase19_execution_resources.c -lsqlite3 $loader_lib $thread_lib -o "$output"
fi

if [ "$selected" = "all" ]; then
  for acceptance_case in names memory rollback batches stream spill lifecycle cancel; do
    "$output" "$extension" "$acceptance_case"
  done
else
  "$output" "$extension" "$selected"
fi
