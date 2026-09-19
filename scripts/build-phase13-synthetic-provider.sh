#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: scripts/build-phase13-synthetic-provider.sh <sqlite-include-dir> <output-path>" >&2
  exit 2
fi

include_dir=$1
output=$2

if [ ! -f "$include_dir/sqlite3ext.h" ]; then
  echo "sqlite3ext.h not found in $include_dir" >&2
  exit 1
fi

mkdir -p "$(dirname -- "$output")"

case "$(uname -s)" in
  Darwin)
    ${CC:-cc} \
      -std=c11 -Wall -Wextra -Werror -fPIC \
      -I"$include_dir" -Iinclude \
      -bundle -undefined dynamic_lookup \
      tests/synthetic_embedding_provider.c \
      -o "$output"
    ;;
  Linux)
    ${CC:-cc} \
      -std=c11 -Wall -Wextra -Werror -fPIC \
      -I"$include_dir" -Iinclude \
      -shared \
      tests/synthetic_embedding_provider.c \
      -o "$output"
    ;;
  *)
    echo "Phase 13 synthetic provider fixture is built by Unix smoke gates" >&2
    exit 2
    ;;
esac
