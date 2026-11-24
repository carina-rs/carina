#!/usr/bin/env bash
# Usage: bash scripts/core-cross-checks.sh [--base <ref>]
# When touched-crates.sh selects --workspace (core or workspace-wide inputs),
# build and require WASM fixtures, run the core/CLI/LSP suites with all features,
# then run core doctests. Catch cross-check regressions before push now that
# full verification runs in CI. CI still runs the full set, including slow tests.
set -euo pipefail
unset CDPATH
cd "$(dirname "${BASH_SOURCE[0]}")/.."

touched=$(bash scripts/touched-crates.sh "$@")
if [[ "$touched" != --workspace ]]; then
    echo 'core-cross-checks.sh: carina-core (or another workspace-wide input) is not touched; nothing was run.'
    exit 0
fi

if [[ -n ${CARGO_TARGET_DIR+x} || -n ${CARGO_BUILD_TARGET_DIR+x} ]]; then
    configured_target=${CARGO_TARGET_DIR-${CARGO_BUILD_TARGET_DIR-}}
    if ! mkdir -p -- "$configured_target" ||
        ! fixture_target=$(cd -- target >/dev/null 2>&1 && pwd -P) ||
        ! cargo_target=$(cd -- "$configured_target" >/dev/null 2>&1 && pwd -P) ||
        [[ "$fixture_target" != "$cargo_target" ]]; then
        cat >&2 <<'MESSAGE'
core-cross-checks.sh: fixture-using e2e tests read target/wasm32-wasip2/debug regardless of CARGO_TARGET_DIR or CARGO_BUILD_TARGET_DIR, so about 25 tests fail spuriously when target does not resolve to the configured cargo target directory.
In the repo root, if target is a real directory, move or remove it first, then run:
MESSAGE
        printf '  ln -s -- %q target\n' "$configured_target" >&2
        exit 1
    fi
fi

# Reuse the fixture package list and build command shared with CI.
bash scripts/verify.sh wasm-fixtures
bash scripts/require-wasm-fixtures.sh

# validate_diagnostic_order and diagnostic_replay are corpus binaries taking
# 10+ minutes per test locally; CI runs both. reference_visit tests exercise
# the 4096/4097 reference limits, take 230-364 s even in CI, and caught none
# of the observed failures. CI still runs all of these tests.
# CI still runs every excluded test; none of the 84 PR CI failures observed
# during 2026-11-16..23 was in an excluded test.
FILTER='not (binary(=validate_diagnostic_order) | binary(=diagnostic_replay) | test(/reference_visit/)'
# Asserts apply exits within its own 30 s wall-clock hang guard. heavy-slot's
# taskpolicy -b uses efficiency cores only: apply took 71 s (53 s on a second
# run), versus 15 s at normal priority, failing every local run. Test totals:
# local 62 s alone (fails); CI 14 s (passes).
FILTER+=' | (binary(=aws_credentials_preflight_e2e) & test(=apply_without_aws_credentials_fails_fast_with_the_typed_startup_error))'
# Starts carina several times, each loading the mock WASM provider; under
# taskpolicy -b it hits the 600 s per-test limit in the 4-thread run. Measured:
# local 381 s alone; parallel run timed out at 600 s; CI 26 s.
FILTER+=' | (binary(=conversion_round3) & test(=r5c_boundary_conversions_stop_before_operational_export_checks))'
# Starts carina several times, each loading the mock WASM provider; under
# taskpolicy -b it hits the 600 s per-test limit in the 4-thread run. Measured:
# local 287 s alone; parallel run timed out at 600 s; CI 31 s.
FILTER+=' | (binary(=conversion_round3) & test(=round3_read_only_commands_accept_the_same_config_with_a_conversion))'
FILTER+=')'

status=0
cargo nextest run -p carina-core -p carina-cli -p carina-lsp \
    --all-features --no-fail-fast -E "$FILTER" || status=1
cargo test --doc -p carina-core --all-features || status=1
exit "$status"
