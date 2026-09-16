#!/usr/bin/env sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: scripts/phase11-adapter-bench.sh <extension-path> <database-path> <read-iterations> <tx-iterations>" >&2
  exit 2
fi

extension=$1
database=$2
reads=$3
transactions=$4
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase11/adapter-bench"
mkdir -p "$(dirname -- "$output")"

dynamic_loader_lib=""
case "$(uname -s)" in
  Darwin) ;;
  Linux) dynamic_loader_lib="-ldl" ;;
esac

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -Iinclude \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/phase11_adapter_bench.c \
  -lsqlite3 $dynamic_loader_lib -pthread \
  -o "$output"

"$output" "$extension" "$database" "$reads" "$transactions"
