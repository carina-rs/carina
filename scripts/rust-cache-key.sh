#!/usr/bin/env bash
# Keep all root manifest bytes in the cache key except the workspace version.
# Use only tools present in both ubuntu-latest and rust:1-bookworm.
set -euo pipefail

manifest=${1:-Cargo.toml}

normalize() {
  # Be conservative with multiline TOML strings: a table-looking line inside
  # one must not hide a change. Such manifests use their complete contents.
  if grep -qE "\"\"\"|'''" "$manifest"; then
    cat "$manifest"
    return
  fi
  awk '
    /^[[:space:]]*\[/ {
      workspace_package = ($0 ~ /^[[:space:]]*\[workspace\.package\][[:space:]]*(#.*)?$/)
    }
    workspace_package && /^[[:space:]]*version[[:space:]]*=[[:space:]]*"[^"\\]*"[[:space:]]*(#.*)?$/ {
      sub(/"[^"\\]*"/, "\"__ci_version__\"")
    }
    { print }
  ' "$manifest"
}

{
  normalize
  # awk adds a record separator to an unterminated last line. Keep a missing
  # final newline distinct instead of accidentally normalizing another edit.
  if [[ $(tail -c 1 "$manifest" | wc -l) -eq 0 ]]; then
    printf '\0no-final-newline'
  fi
} | shasum -a 256 | cut -d ' ' -f 1
