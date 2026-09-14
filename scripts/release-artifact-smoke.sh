#!/usr/bin/env sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: scripts/release-artifact-smoke.sh <extension-path> [interop-fixture]" >&2
  exit 2
fi

extension=$1
interop_fixture=${2:-}
if [ ! -f "$extension" ]; then
  echo "extension artifact does not exist: $extension" >&2
  exit 1
fi

sqlite_bin=${LITHOGRAPH_SQLITE3:-$(command -v sqlite3 || true)}
if [ -z "$sqlite_bin" ] || [ ! -x "$sqlite_bin" ]; then
  echo "SQLite CLI is required for release artifact smoke" >&2
  exit 1
fi
export LITHOGRAPH_SQLITE3="$sqlite_bin"

cargo run --locked --quiet -p lithograph-test-support --bin lithograph-sqlite-probe -- "$extension"
for phase in 01 02 03 04 05 06 07 08 09; do
  cargo run --locked --quiet -p lithograph-test-support --bin "lithograph-phase$phase" -- "$extension"
done

python3 scripts/check-extension-artifact.py "$extension"
scripts/native-abi-smoke.sh "$extension"

# Same artifact, two independently built host runtimes: the frozen minimum and
# the current release candidate. These probes intentionally focus on
# load/init/read/write because the full feature/ABI smoke above already ran on
# the native runner runtime.
scripts/sqlite-345-smoke.sh "$extension"
scripts/sqlite-3534-smoke.sh "$extension"

if [ -n "$interop_fixture" ]; then
  cargo run --locked --quiet -p lithograph-test-support \
    --bin lithograph-phase10-storage-fixture -- \
    verify "$interop_fixture" "$extension"
fi
