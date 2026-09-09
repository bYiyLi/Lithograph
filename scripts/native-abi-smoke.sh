#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/native-abi-smoke.sh <extension-path>" >&2
  exit 2
fi

extension=$1
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase01/native-abi-smoke"
mkdir -p "$(dirname -- "$output")"

dynamic_loader_lib=""
if [ "$(uname -s)" = "Linux" ]; then
  dynamic_loader_lib="-ldl"
fi

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -Iinclude \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/native_abi_smoke.c \
  -lsqlite3 $dynamic_loader_lib \
  -o "$output"

"$output" "$extension"
