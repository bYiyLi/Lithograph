#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: scripts/native-scale-smoke.sh <extension-path> <database-path>" >&2
  exit 2
fi

extension=$1
database=$2
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase10/native-scale-smoke"
mkdir -p "$(dirname -- "$output")"

dynamic_loader_lib=""
case "$(uname -s)" in
  Linux) dynamic_loader_lib="-ldl" ;;
esac

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -Iinclude \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/native_scale_smoke.c \
  -lsqlite3 $dynamic_loader_lib \
  -o "$output"

"$output" "$extension" "$database"
