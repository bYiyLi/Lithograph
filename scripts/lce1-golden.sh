#!/usr/bin/env sh
set -eu

expected_bytes='4c434531054c41594552041c4c434531044e4f444502080700000000000000080100000000000000264c434531054c4142454c03080700000000000000080300000000000000080100000000000000364c4345310352454c05080b0000000000000008070000000000000008050000000000000008090000000000000008010000000000000094014c4345310850524f5045525459060801000000000000000807000000000000000804000000000000000801000000000000000901050000000000000057014c4345310556414c55450201054803164c4345310556414c554502010208efffffffffffffff184c4345310556414c55450201040a4c6974686f6772617068164c4345310556414c5545020103080000000000000080'
expected_hash='cea822c1c96dd7c456beae36aa4e9e89d8d3cad8658aa450b3ede2887ade4440'

encode_once() {
  cargo run --locked --quiet -p lithograph-test-support --bin lithograph-lce1-golden
}

first=$(encode_once)
second=$(encode_once)

[ "$first" = "$second" ] || {
  echo 'LCE1 encoding differs across independent processes' >&2
  exit 1
}

actual_bytes=$(printf '%s\n' "$first" | sed -n '1p')
actual_hash=$(printf '%s\n' "$first" | sed -n '2p')
[ "$actual_bytes" = "$expected_bytes" ] || {
  echo 'LCE1 golden bytes changed; storage format bump/migration is required' >&2
  exit 1
}
[ "$actual_hash" = "$expected_hash" ] || {
  echo 'LCE1 golden hash changed; storage format bump/migration is required' >&2
  exit 1
}

echo "LCE1 golden vector passed: $actual_hash"
