#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
checker="$repo_root/scripts/check-error-source-not-in-display.sh"
fixtures="$repo_root/scripts/tests/error-source-not-in-display"
output=$(mktemp)
trap 'rm -f "$output"' EXIT

if bash "$checker" "$fixtures/positive.rs" "$fixtures/negative.rs" >"$output" 2>&1; then
  echo "checker unexpectedly accepted positive fixtures" >&2
  cat "$output" >&2
  exit 1
fi

for marker in \
  POS_DUP_EXPLICIT_NAMED \
  POS_INFO_LOSS \
  POS_TUPLE_INDEX_ONE \
  POS_TUPLE_DOT_ONE \
  POS_SAME_LINE_NAMED \
  POS_IMPLICIT_SOURCE \
  POS_POSITIONAL_FIELD \
  POS_TRAILING_COMMENT \
  POS_FROM_INFO_LOSS \
  POS_CALLER_RENDERED_DUP
do
  if ! grep -q "$marker" "$output"; then
    echo "checker did not flag fixture $marker" >&2
    cat "$output" >&2
    exit 1
  fi
done

if grep -q 'negative.rs' "$output"; then
  echo "checker reported a negative fixture" >&2
  cat "$output" >&2
  exit 1
fi

if ! grep -q 'duplicates source in Display (1).*POS_TUPLE_INDEX_ONE' "$output"; then
  echo "checker did not resolve tuple field index 1" >&2
  cat "$output" >&2
  exit 1
fi

if ! grep -q 'omits source from Display.*POS_INFO_LOSS' "$output"; then
  echo "checker did not classify information loss" >&2
  cat "$output" >&2
  exit 1
fi

echo "OK: terminal-error checker fixtures passed."
