#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

matches=$(
  grep -rnE \
    --include='*.rs' \
    --exclude-dir=target \
    --exclude-dir=.worktrees \
    'var(_os)?[[:space:]]*\([[:space:]]*"(HOME|USERPROFILE)"[[:space:]]*\)' \
    "$repo_root" || true
)

if [[ -n "$matches" ]]; then
  echo "Direct HOME environment reads are forbidden; use carina_core::UserDirs." >&2
  echo "$matches" >&2
  exit 1
fi

echo "No direct HOME environment reads found."
