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
sh scripts/check-sqlite-host-abi.sh
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
  scripts/phase19-execution-resources.sh "$extension"
  python3 -B tests/phase19_resources_sql.py "$extension"
  python3 -B tests/phase21_predicate_routing_sql.py "$extension"
  python3 -B tests/phase22_access_sql.py "$extension"
  python3 -B tests/phase22_checkpoint_sql.py "$extension"
  python3 -B tests/phase22_statistics_sql.py "$extension"
  python3 -B tests/phase22_query_only_sql.py "$extension"
  python3 -B tests/phase22_relationship_work.py --semantics-only "$extension"
  python3 -B tests/phase22_join_work.py --semantics-only "$extension"
  python3 -B tests/phase22_hash_work.py --semantics-only "$extension"
  python3 -B tests/phase22_join_manifest_sql.py "$extension"
  sh scripts/phase22-access.sh "$extension"
  python3 -B tests/phase21_predicate_routing_bench.py "$extension" 1000
  vm_results="$target_dir/phase21/vm-work-results.jsonl"
  mkdir -p "$target_dir/phase21"
  : > "$vm_results"
  for size in 1000 10000; do
    vm_fixture="$target_dir/phase21/vm-work-$size.sqlite3"
    rm -f "$vm_fixture" "$vm_fixture-wal" "$vm_fixture-shm"
    python3 -B tests/phase21_predicate_routing_bench.py \
      --seed-worker "$extension" "$vm_fixture" "$size" >/dev/null
    cargo run --locked --quiet -p lithograph-test-support \
      --bin lithograph-phase21-checkpoint -- "$vm_fixture" >/dev/null
    sh scripts/phase21-vm-work.sh "$extension" "$vm_fixture" >> "$vm_results"
  done
  python3 -B - "$vm_results" <<'PY'
import json
import sys

small, large = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
assert small["sqlite"] == large["sqlite"], "SQLite runtime changed during VM comparison"
assert large["vm_steps"] <= 2 * small["vm_steps"] + 1000, (
    "10x unrelated nodes inflated SQLite VM work", small, large
)
assert large["fullscan_steps"] <= 2 * small["fullscan_steps"] + 1000, (
    "10x unrelated nodes inflated SQLite full scans", small, large
)
print("Phase 21 checkpointed SQLite VM scaling: passed", small, large)
PY
  python3 -B tests/phase22_factorized_sql.py "$extension" \
    "$target_dir/debug/lithograph-phase21-checkpoint"
  phase22_history_dir=$(mktemp -d "$target_dir/phase22-history.XXXXXX")
  python3 -B tests/phase22_history_work.py "$extension" "$phase22_history_dir" \
    --checkpoint-tool "$target_dir/debug/lithograph-phase21-checkpoint" \
    --vm-tool "$target_dir/phase21/vm-work" --nodes 32 --commits 10 --reopens 2
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
  python3 -B tests/phase18_access_sql.py "$extension"
  python3 -B tests/phase18_load_csv_access.py "$extension"
  scripts/phase18-external-access-smoke.sh "$extension"
  scripts/phase15-provider-cache-smoke.sh "$openai_provider" "$extension"

  # The language examples come from Markdown, with assertions and setup references
  # in sidecars. Synthetic embedding exercises the real dual-extension boundary
  # without contacting a service or consuming credentials.
  case "$(uname -s)" in
    Darwin) docs_provider="$target_dir/phase10/sqlite-3.53.4/phase13_synthetic_embedding_provider.dylib" ;;
    Linux) docs_provider="$target_dir/phase10/sqlite-3.53.4/phase13_synthetic_embedding_provider.so" ;;
  esac
  python3 -B docs/guide/examples/check_docs.py
  python3 -B docs/guide/examples/verify_cypher.py --self-test
  python3 -B docs/guide/examples/generate_reference.py --extension "$extension" \
    --sqlite3 "$LITHOGRAPH_SQLITE3" --check
  python3 -B docs/guide/examples/verify_cypher.py "$extension" \
    --sqlite3 "$LITHOGRAPH_SQLITE3" --provider "$docs_provider"
  python3 -B docs/guide/examples/verify.py "$extension" --sqlite3 "$LITHOGRAPH_SQLITE3"
  python3 -B docs/guide/examples/python_quickstart.py "$extension"
  python3 -B docs/guide/examples/version_workflow.py "$extension"
  python3 -B docs/guide/examples/sql_transaction.py "$extension"
  python3 -B docs/guide/examples/streaming.py "$extension"

  docs_source="$target_dir/phase10/sqlite-3.53.4/sqlite-autoconf-3530400"
  docs_output="$target_dir/developer-docs/native-transaction"
  mkdir -p "$(dirname -- "$docs_output")"
  ${CC:-cc} -std=c11 -Wall -Wextra -Werror -I"$docs_source" \
    -c docs/guide/examples/native_transaction.c -o "$docs_output.o"
  ${CC:-cc} -std=c11 -w -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 \
    -I"$docs_source" -c "$docs_source/sqlite3.c" -o "$docs_output.sqlite3.o"
  docs_loader_lib=""
  if [ "$(uname -s)" = "Linux" ]; then docs_loader_lib="-ldl"; fi
  ${CC:-cc} "$docs_output.o" "$docs_output.sqlite3.o" \
    $docs_loader_lib -pthread -lm -o "$docs_output"
  "$docs_output" "$extension"
fi

cargo run --locked --quiet -p lithograph-test-support --bin lithograph-compat -- self-check
cargo run --locked --quiet -p lithograph-test-support --bin lithograph-compat -- inventory tests/fixtures/cypher25
cargo run --locked --quiet -p lithograph-test-support --bin lithograph-compat -- tck-inventory
