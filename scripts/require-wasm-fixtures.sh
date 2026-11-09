#!/usr/bin/env bash
# Missing fixtures make some integration tests return early. Fail CI instead.
set -euo pipefail
cd "$(dirname "$0")/.."

for fixture in \
  carina-provider-mock \
  carina-provider-mock-raw-float \
  carina-provider-mock-http \
  carina-provider-mock-http-aws \
  carina-provider-mock-http-aws-unused \
  carina-provider-mock-http-sdk-only; do
  path="target/wasm32-wasip2/debug/$fixture.wasm"
  if [[ ! -s "$path" ]]; then
    echo "ERROR: required WASM fixture is missing or empty: $path" >&2
    exit 1
  fi
done
