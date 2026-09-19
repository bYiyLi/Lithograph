#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: scripts/phase13-semantic-performance.sh <lithograph-extension> <synthetic-provider>" >&2
  exit 2
fi

lithograph=$1
provider=$2
sqlite_bin=$(printenv LITHOGRAPH_SQLITE3 || printf '%s' sqlite3)
sqlite_source_dir=$(printenv LITHOGRAPH_SQLITE_SOURCE_DIR || true)
target_dir=$(printenv CARGO_TARGET_DIR || printf '%s' target)
cc_bin=$(printenv CC || printf '%s' cc)
report_dir="$target_dir/phase13-performance"
output="$report_dir/semantic-performance"
database="$report_dir/semantic-performance.db"
report="$report_dir/provider-cache.json"
mkdir -p "$report_dir"

case "$(uname -s)" in
  Darwin)
    dynamic_loader_lib=""
    ;;
  Linux)
    dynamic_loader_lib="-ldl"
    ;;
  *)
    echo "Phase 13 semantic performance evidence is supported on macOS and Linux" >&2
    exit 2
    ;;
esac

if [ -n "$sqlite_source_dir" ]; then
  smoke_object="$output.o"
  sqlite_object="$output.sqlite3.o"
  "$cc_bin" \
    -std=c11 -Wall -Wextra -Werror -D_DARWIN_C_SOURCE \
    -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_source_dir" -Iinclude \
    tests/phase13_semantic_performance.c \
    -c -o "$smoke_object"
  "$cc_bin" \
    -std=c11 -w \
    -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$sqlite_source_dir" \
    "$sqlite_source_dir/sqlite3.c" \
    -c -o "$sqlite_object"
  "$cc_bin" "$smoke_object" "$sqlite_object" \
    $dynamic_loader_lib -pthread -lm -o "$output"
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  "$cc_bin" \
    -std=c11 -Wall -Wextra -Werror -D_DARWIN_C_SOURCE \
    -I"$sqlite_prefix/include" -Iinclude \
    -L"$sqlite_prefix/lib" \
    tests/phase13_semantic_performance.c \
    -lsqlite3 $dynamic_loader_lib -pthread -lm \
    -o "$output"
fi

"$output" "$lithograph" "$provider" "$database" >"$report"
python3 -m json.tool "$report" >/dev/null
cat "$report"
