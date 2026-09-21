#!/usr/bin/env sh
set -eu

is_supported_sqlite_version() {
  version="$1"
  major=$(printf '%s' "$version" | cut -d. -f1)
  minor=$(printf '%s' "$version" | cut -d. -f2)
  [ "$major" -gt 3 ] || { [ "$major" -eq 3 ] && [ "$minor" -ge 45 ]; }
}

sqlite_is_usable() {
  candidate="$1"
  [ -x "$candidate" ] || return 1
  version=$($candidate :memory: 'SELECT sqlite_version();' 2>/dev/null) || return 1
  is_supported_sqlite_version "$version" || return 1
  options=$($candidate :memory: 'PRAGMA compile_options;' 2>/dev/null) || return 1
  printf '%s\n' "$options" | grep -q '^ENABLE_FTS5$' || return 1
  printf '%s\n' "$options" | grep -Eq '^THREADSAFE=[12]$' || return 1
  if printf '%s\n' "$options" | grep -q '^OMIT_LOAD_EXTENSION$'; then
    return 1
  fi
}

select_sqlite() {
  if [ -n "${LITHOGRAPH_SQLITE3:-}" ]; then
    sqlite_is_usable "$LITHOGRAPH_SQLITE3" || {
      echo "LITHOGRAPH_SQLITE3 does not satisfy SQLite 3.45+, FTS5, thread-safety, and load-extension requirements" >&2
      exit 1
    }
    return
  fi

  default_sqlite=$(command -v sqlite3 2>/dev/null || true)
  for candidate in "$default_sqlite" /opt/homebrew/opt/sqlite/bin/sqlite3 /usr/local/opt/sqlite/bin/sqlite3; do
    if [ -n "$candidate" ] && sqlite_is_usable "$candidate"; then
      LITHOGRAPH_SQLITE3="$candidate"
      export LITHOGRAPH_SQLITE3
      return
    fi
  done

  echo "No SQLite runtime satisfies SQLite 3.45+, FTS5, thread-safety, and load-extension requirements" >&2
  exit 1
}

select_sqlite
echo "Using SQLite: $LITHOGRAPH_SQLITE3 ($($LITHOGRAPH_SQLITE3 --version))"

python3 scripts/check-vendor.py
scripts/lce1-golden.sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features
# `rusqlite/loadable_extension` switches SQLite calls to the host API table.
# Cargo feature unification would force that ABI mode into standalone Core
# integration tests if all packages were tested in one workspace invocation.
cargo test --locked -p lithograph-core -p lithograph-test-support
cargo test --locked -p lithograph-extension
cargo test --locked -p lithograph-embedding-provider -p lithograph-openai-compatible
cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase10-tck >/dev/null
cargo build --locked -p lithograph-extension
cargo build --locked -p lithograph-openai-compatible

target_dir=${CARGO_TARGET_DIR:-target}
case "$(uname -s)" in
  Darwin)
    built_extension="$target_dir/debug/liblithograph.dylib"
    extension="$target_dir/debug/lithograph.dylib"
    openai_provider="$target_dir/debug/liblithograph_openai_compatible.dylib"
    ;;
  Linux)
    built_extension="$target_dir/debug/liblithograph.so"
    extension="$target_dir/debug/lithograph.so"
    openai_provider="$target_dir/debug/liblithograph_openai_compatible.so"
    ;;
  *)
    built_extension=""
    extension=""
    openai_provider=""
    ;;
esac

if [ -n "$extension" ]; then
  cp "$built_extension" "$extension"
  extension_without_suffix=${extension%.*}
  "$LITHOGRAPH_SQLITE3" -batch -noheader \
    -cmd ".load $extension_without_suffix" \
    :memory: \
    'SELECT json_valid(lithograph_version());' | grep -qx '1'
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-sqlite-probe -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase01 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase02 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase03 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase04 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase05 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase06 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase07 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase08 -- "$extension"
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase09 -- "$extension"
  scripts/sql-tx-smoke.sh "$extension"
  scripts/openai-compatible-provider-smoke.sh "$openai_provider"
  scripts/sqlite-345-smoke.sh "$extension" "$openai_provider"
  scripts/sqlite-3534-smoke.sh "$extension" "$openai_provider"
  case "$(uname -s)" in
    Darwin) phase12_tokenizer="$target_dir/phase10/sqlite-3.53.4/phase12_tokenizer.dylib" ;;
    Linux) phase12_tokenizer="$target_dir/phase10/sqlite-3.53.4/phase12_tokenizer.so" ;;
    *) phase12_tokenizer="" ;;
  esac
  if [ -n "$phase12_tokenizer" ]; then
    cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase12 -- "$extension" "$phase12_tokenizer"
  fi
  python3 scripts/check-extension-artifact.py "$extension"
  python3 scripts/check-extension-artifact.py "$openai_provider" --provider
  scripts/sql-surface-smoke.sh "$extension"
  scripts/phase15-provider-cache-smoke.sh "$openai_provider"
fi

cargo run --locked --quiet -p lithograph-test-support --bin lithograph-compat -- self-check
cargo run --locked --quiet -p lithograph-test-support --bin lithograph-compat -- inventory tests/fixtures/cypher25
cargo run --locked --quiet -p lithograph-test-support --bin lithograph-compat -- tck-inventory
