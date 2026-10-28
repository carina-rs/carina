#!/usr/bin/env sh
set -eu

if [ -z "${NEXTEST_ENV:-}" ]; then
  echo "NEXTEST_ENV is required for the Carina nextest setup script" >&2
  exit 1
fi

test_root=$(mktemp -d "${TMPDIR:-/tmp}/carina-nextest.XXXXXX")
test_home="$test_root/home"
mkdir -p \
  "$test_home/.carina/trust" \
  "$test_home/.carina/plugin-cache" \
  "$test_home/.carina/cache" \
  "$test_home/.agents/skills"

{
  printf 'HOME=%s\n' "$test_home"
  printf 'CARINA_TRUST_RECORD_DIR=%s\n' "$test_home/.carina/trust"
  printf 'CARINA_PLUGIN_CACHE_DIR=%s\n' "$test_home/.carina/plugin-cache"
  printf 'CARINA_TEST_HOME=%s\n' "$test_home"
} >> "$NEXTEST_ENV"
