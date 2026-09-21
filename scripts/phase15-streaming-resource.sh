#!/usr/bin/env sh
set -eu

if [ "$#" -ne 5 ]; then
  echo "usage: scripts/phase15-streaming-resource.sh <extension-path> <database-path> <read|tx|txcsv|baseline|fast|slow|early> <rows> <delay-micros>" >&2
  exit 2
fi

extension=$1
database=$2
mode=$3
rows=$4
delay_micros=$5
sqlite_bin=${LITHOGRAPH_SQLITE3:-sqlite3}
sqlite_prefix=$(CDPATH= cd -- "$(dirname -- "$sqlite_bin")/.." && pwd)
output="${CARGO_TARGET_DIR:-target}/phase15/streaming-resource"
mkdir -p "$(dirname -- "$output")"

${CC:-cc} \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I"$sqlite_prefix/include" \
  -L"$sqlite_prefix/lib" \
  tests/phase15_streaming_resource.c \
  -lsqlite3 \
  -o "$output"

case "$(uname -s)" in
  Darwin)
    exec /usr/bin/time -l "$output" "$extension" "$database" "$mode" "$rows" "$delay_micros"
    ;;
  Linux)
    exec /usr/bin/time -v "$output" "$extension" "$database" "$mode" "$rows" "$delay_micros"
    ;;
  *)
    exec "$output" "$extension" "$database" "$mode" "$rows" "$delay_micros"
    ;;
esac
