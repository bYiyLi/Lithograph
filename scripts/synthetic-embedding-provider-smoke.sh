#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/synthetic-embedding-provider-smoke.sh <provider-extension-path>" >&2
  exit 2
fi

provider=$1
sqlite_bin=$(printenv LITHOGRAPH_SQLITE3 || printf '%s' sqlite3)
sqlite_source_dir=$(printenv LITHOGRAPH_SQLITE_SOURCE_DIR || true)
target_dir=$(printenv CARGO_TARGET_DIR || printf '%s' target)
cc_bin=$(printenv CC || printf '%s' cc)
output="$target_dir/phase13/synthetic-embedding-provider-smoke"
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
  smoke_object="$output.o"
  sqlite_object="$output.sqlite3.o"
  "$cc_bin" -std=c11 -Wall -Wextra -Werror -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 -Iinclude -I"$sqlite_source_dir" tests/synthetic_embedding_provider_smoke.c -c -o "$smoke_object"
  "$cc_bin" -std=c11 -w -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 -I"$sqlite_source_dir" "$sqlite_source_dir/sqlite3.c" -c -o "$sqlite_object"
  "$cc_bin" "$smoke_object" "$sqlite_object" $dynamic_loader_lib $thread_lib -lm -o "$output"
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  "$cc_bin" -std=c11 -Wall -Wextra -Werror -Iinclude -I"$sqlite_prefix/include" -L"$sqlite_prefix/lib" tests/synthetic_embedding_provider_smoke.c -lsqlite3 $dynamic_loader_lib $thread_lib -lm -o "$output"
fi

"$output" "$provider"
