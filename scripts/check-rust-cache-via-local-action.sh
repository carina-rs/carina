#!/usr/bin/env bash
# Require CI workflows to use the local Rust cache action so the root virtual
# manifest is always included in the cache key.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

# release.yml is frozen by project rule; see #3963.
matches=$(
  grep -RInE \
    --exclude='release.yml' \
    "^[[:space:]]*(-[[:space:]]*)?uses:[[:space:]]*['\"]?Swatinem/rust-cache(@|['\"]|[[:space:]]|$)" \
    .github/workflows || true
)

if [[ -n "$matches" ]]; then
  echo "ERROR: Workflows must use ./.github/actions/rust-cache instead of Swatinem/rust-cache directly:" >&2
  echo "$matches" >&2
  exit 1
fi

echo "OK: All applicable workflows use the local Rust cache action."
