#!/usr/bin/env bash
# These tests commit only inside disposable repositories, never this worktree.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 "$repo_root/scripts/tests/classify-ci-diff/test_classifier.py" \
  "${1:-$repo_root/scripts/classify-ci-diff.sh}"
python3 "$repo_root/scripts/tests/classify-ci-diff/test_workflow_conditions.py" \
  "$repo_root/.github/workflows/ci.yml"
