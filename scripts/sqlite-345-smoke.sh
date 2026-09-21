#!/usr/bin/env sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: scripts/sqlite-345-smoke.sh <extension-path> [openai-compatible-provider-path]" >&2
  exit 2
fi

extension=$1
openai_provider=${2:-}
root="${CARGO_TARGET_DIR:-target}/phase01/sqlite-3.45.0"
archive="$root/sqlite-autoconf-3450000.tar.gz"
source_dir="$root/sqlite-autoconf-3450000"
sqlite_bin="$root/sqlite3"
expected_sha256="72887d57a1d8f89f52be38ef84a6353ce8c3ed55ada7864eb944abd9a495e436"

mkdir -p "$root"

if [ ! -f "$archive" ]; then
  curl -fsSL \
    https://www.sqlite.org/2024/sqlite-autoconf-3450000.tar.gz \
    -o "$archive"
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual_sha256=$(sha256sum "$archive" | awk '{print $1}')
else
  actual_sha256=$(shasum -a 256 "$archive" | awk '{print $1}')
fi

if [ "$actual_sha256" != "$expected_sha256" ]; then
  echo "SQLite 3.45.0 fixture checksum mismatch" >&2
  exit 1
fi

if [ ! -d "$source_dir" ]; then
  tar -xzf "$archive" -C "$root"
fi

dynamic_loader_lib=""
if [ "$(uname -s)" = "Linux" ]; then
  dynamic_loader_lib="-ldl"
fi

${CC:-cc} \
  -O2 \
  -DSQLITE_THREADSAFE=1 \
  -DSQLITE_ENABLE_FTS5 \
  "$source_dir/sqlite3.c" \
  "$source_dir/shell.c" \
  -lpthread -lm $dynamic_loader_lib \
  -o "$sqlite_bin"

version=$($sqlite_bin :memory: 'SELECT sqlite_version();')
if [ "$version" != "3.45.0" ]; then
  echo "expected SQLite 3.45.0 fixture, got $version" >&2
  exit 1
fi

case "$(uname -s)" in
  Darwin) tokenizer_extension="$root/phase12_tokenizer.dylib" ;;
  Linux) tokenizer_extension="$root/phase12_tokenizer.so" ;;
  *) tokenizer_extension="" ;;
esac
if [ -n "$tokenizer_extension" ]; then
  scripts/build-phase12-tokenizer.sh "$source_dir" "$tokenizer_extension"
fi

case "$(uname -s)" in
  Darwin) synthetic_provider="$root/phase13_synthetic_embedding_provider.dylib" ;;
  Linux) synthetic_provider="$root/phase13_synthetic_embedding_provider.so" ;;
  *) synthetic_provider="" ;;
esac
if [ -n "$synthetic_provider" ]; then
  scripts/build-phase13-synthetic-provider.sh "$source_dir" "$synthetic_provider"
fi

LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-sqlite-probe -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase01 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase02 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase03 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase04 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase05 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase06 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase07 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase08 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase09 -- "$extension"
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  LITHOGRAPH_SQLITE_SOURCE_DIR="$source_dir" \
  scripts/sql-tx-smoke.sh "$extension"
if [ -n "$tokenizer_extension" ]; then
  LITHOGRAPH_SQLITE3="$sqlite_bin" \
    cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase12 -- "$extension" "$tokenizer_extension"
fi
LITHOGRAPH_SQLITE3="$sqlite_bin" \
  LITHOGRAPH_SQLITE_SOURCE_DIR="$source_dir" \
  scripts/sql-surface-smoke.sh "$extension"
if [ -n "$openai_provider" ]; then
  LITHOGRAPH_SQLITE3="$sqlite_bin" \
    LITHOGRAPH_SQLITE_SOURCE_DIR="$source_dir" \
    scripts/openai-compatible-provider-smoke.sh "$openai_provider"
  LITHOGRAPH_SQLITE3="$sqlite_bin" \
    LITHOGRAPH_SQLITE_SOURCE_DIR="$source_dir" \
    scripts/phase15-provider-cache-smoke.sh "$openai_provider"
fi
if [ -n "$synthetic_provider" ]; then
  LITHOGRAPH_SQLITE3="$sqlite_bin" \
    LITHOGRAPH_SQLITE_SOURCE_DIR="$source_dir" \
    scripts/synthetic-embedding-provider-smoke.sh "$synthetic_provider"
  if [ -n "$openai_provider" ]; then
    LITHOGRAPH_SQLITE3="$sqlite_bin" \
      cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase13 -- \
      "$extension" "$synthetic_provider" "$openai_provider"
  else
    LITHOGRAPH_SQLITE3="$sqlite_bin" \
      cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase13 -- \
      "$extension" "$synthetic_provider"
  fi
  LITHOGRAPH_SQLITE3="$sqlite_bin" \
    LITHOGRAPH_SQLITE_SOURCE_DIR="$source_dir" \
    scripts/phase13-semantic-concurrency.sh "$extension" "$synthetic_provider"
fi
