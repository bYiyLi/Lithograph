#!/usr/bin/env sh
set -eu

# RUSTSEC-2026-0459 applies to libsqlite3-sys's bundled SQLite copy.
# The exception in deny.toml is valid only while the workspace always
# resolves SQLite symbols through the host loadable-extension API.
# Fail closed if the dependency or any target/feature changes.
# Also fail closed when the documented temporary review period ends.
review_deadline=20270208
today=$(date -u +%Y%m%d)
if [ "$today" -ge "$review_deadline" ]; then
  echo "RUSTSEC-2026-0459 exception reached its 2027-02-08 review deadline" >&2
  exit 1
fi
features=$(cargo tree --locked --all-features --target all --edges features --invert libsqlite3-sys@0.38.2) || {
  echo "SQLite dependency changed; re-review RUSTSEC-2026-0459 and deny.toml" >&2
  exit 1
}
printf '%s\n' "$features" | grep -Fq 'rusqlite v0.40.1' || {
  echo "rusqlite dependency changed; re-review the SQLite ABI/security exception" >&2
  exit 1
}
printf '%s\n' "$features" | grep -Fq 'libsqlite3-sys feature "loadable_extension"' || {
  echo "host SQLite loadable-extension feature is missing" >&2
  exit 1
}
if printf '%s\n' "$features" | grep -Eq 'libsqlite3-sys feature "bundled[^"]*"'; then
  echo "bundled SQLite enabled: RUSTSEC-2026-0459 exception is not applicable" >&2
  exit 1
fi
echo "SQLite host ABI dependency/feature contract: passed"
