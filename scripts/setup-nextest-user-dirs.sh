#!/usr/bin/env sh
set -eu

if [ -z "${NEXTEST_ENV:-}" ]; then
  echo "NEXTEST_ENV is required for the Carina nextest setup script" >&2
  exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
workspace_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

case ${CARGO_TARGET_DIR:-} in
  "") target_dir="$workspace_root/target" ;;
  /*) target_dir=$CARGO_TARGET_DIR ;;
  *) target_dir="$workspace_root/$CARGO_TARGET_DIR" ;;
esac

runs_root="$target_dir/carina-nextest-home"
mkdir -p "$runs_root"
runs_root=$(CDPATH= cd -P -- "$runs_root" && pwd -P)
# Setup scripts do not receive a teardown hook. Reclaim only completed run
# directories old enough that an overlapping nextest invocation cannot own them.
find "$runs_root" -mindepth 1 -maxdepth 1 -type d -mtime +1 -exec rm -rf {} +

test_root=$(mktemp -d "$runs_root/run.XXXXXX")
test_home="$test_root/home"
mkdir -p \
  "$test_home/tmp" \
  "$test_home/.carina/trust" \
  "$test_home/.carina/plugin-cache" \
  "$test_home/.carina/cache" \
  "$test_home/.agents/skills"

{
  printf 'HOME=%s\n' "$test_home"
  # Keep TempDir-based per-test overrides inside the guarded nextest home.
  printf 'TMPDIR=%s\n' "$test_home/tmp"
  printf 'CARINA_TRUST_RECORD_DIR=%s\n' "$test_home/.carina/trust"
  printf 'CARINA_PLUGIN_CACHE_DIR=%s\n' "$test_home/.carina/plugin-cache"
  printf 'CARINA_TEST_HOME=%s\n' "$test_home"
} >> "$NEXTEST_ENV"
