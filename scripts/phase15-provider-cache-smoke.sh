#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/phase15-provider-cache-smoke.sh <openai-compatible-provider-path>" >&2
  exit 2
fi

provider=$1
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_source_dir=${LITHOGRAPH_SQLITE_SOURCE_DIR:-}
target_dir=${CARGO_TARGET_DIR:-target}
output_dir="$target_dir/phase15"
mkdir -p "$output_dir"

case "$(uname -s)" in
  Darwin)
    probe="$output_dir/provider-cache-probe.dylib"
    link_flags="-dynamiclib -undefined dynamic_lookup"
    ;;
  *)
    probe="$output_dir/provider-cache-probe.so"
    link_flags="-shared"
    ;;
esac

if [ -n "$sqlite_source_dir" ]; then
  if [ ! -f "$sqlite_source_dir/sqlite3.h" ] || [ ! -f "$sqlite_source_dir/sqlite3ext.h" ]; then
    echo "SQLite headers not found in $sqlite_source_dir" >&2
    exit 1
  fi
  sqlite_include=$sqlite_source_dir
else
  sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
  sqlite_include="$sqlite_prefix/include"
fi

${CC:-cc} \
  -std=c11 -Wall -Wextra -Werror -fPIC \
  -Iinclude -I"$sqlite_include" \
  tests/provider_cache_probe.c \
  $link_flags \
  -o "$probe"

cargo run --locked --quiet \
  -p lithograph-test-support \
  --bin lithograph-phase15-provider-cache \
  -- "$provider" "$probe"
