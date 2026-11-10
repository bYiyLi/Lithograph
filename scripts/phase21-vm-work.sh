#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: sh scripts/phase21-vm-work.sh <extension> <synthetic-database>" >&2
  exit 2
fi
extension=$1
database=$2
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase21/vm-work"
mkdir -p "$(dirname -- "$output")"
loader_lib=""
if [ "$(uname -s)" = "Linux" ]; then loader_lib="-ldl"; fi
${CC:-cc} -std=c11 -Wall -Wextra -Werror -O2 \
  -I"$sqlite_prefix/include" -L"$sqlite_prefix/lib" \
  -Wl,-rpath,"$sqlite_prefix/lib" \
  tests/phase21_vm_work.c -lsqlite3 $loader_lib -o "$output"
"$output" "$extension" "$database"
