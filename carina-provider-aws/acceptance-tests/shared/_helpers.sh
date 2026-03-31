#!/bin/bash
# Common helpers for acceptance tests
#
# Usage: source this file from test scripts:
#   source "$(dirname "$0")/../../shared/_helpers.sh"

set -e

HELPERS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Derive SCRIPT_DIR from the caller's location (two levels up from shared/)
# Each test script is at <suite>/tests/<script>.sh, so SCRIPT_DIR is <suite>/
if [ -z "$SCRIPT_DIR" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[1]}")/.." && pwd)"
fi
PROJECT_ROOT="$(cd "$HELPERS_DIR/../../.." && pwd)"

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "Building carina..."
    cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml" --quiet 2>/dev/null \
        || cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml"
fi

# ── Provider source injection ────────────────────────────────────────
AWS_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-aws"
AWSCC_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-awscc"

# inject_provider_source: Create a temp copy of a .crn file with source/version
# injected into provider blocks. Prints the temp file path.
# Args: original_crn_file
inject_provider_source() {
    local original="$1"
    local tmp_file
    tmp_file=$(mktemp).crn

    sed \
        -e '/^provider aws {/a\
  source = "file://'"$AWS_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        -e '/^provider awscc {/a\
  source = "file://'"$AWSCC_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        "$original" > "$tmp_file"

    echo "$tmp_file"
}

# inject_provider_source_dir: Inject source/version into all .crn files in a directory (in-place).
# Idempotent: skips files that already contain 'source =' on the line after a provider block.
# Args: directory
inject_provider_source_dir() {
    local dir="$1"
    find "$dir" -name '*.crn' -print0 | while IFS= read -r -d '' crn_file; do
        if grep -q '^\s*source\s*=' "$crn_file" 2>/dev/null; then
            continue
        fi
        sed -i '' \
            -e '/^provider aws {/a\
  source = "file://'"$AWS_PROVIDER_BIN"'"\
  version = "0.1.0"' \
            -e '/^provider awscc {/a\
  source = "file://'"$AWSCC_PROVIDER_BIN"'"\
  version = "0.1.0"' \
            "$crn_file"
    done
}

TEST_PASSED=0
TEST_FAILED=0

# prepare_work_dir: Inject provider source into all .crn files in a work directory.
# Call this after copying .crn files to the work dir but before running carina commands.
# Args: work_dir
prepare_work_dir() {
    inject_provider_source_dir "$1"
}

ACTIVE_WORK_DIR=""
cleanup() {
    if [ -n "$ACTIVE_WORK_DIR" ] && [ -d "$ACTIVE_WORK_DIR" ]; then
        echo "  Cleanup: destroying resources..."
        cd "$ACTIVE_WORK_DIR"
        "$CARINA_BIN" destroy --auto-approve . 2>/dev/null || true
        "$CARINA_BIN" destroy --auto-approve . 2>/dev/null || true
        rm -f carina.state.json carina.state.lock
    fi
}
trap cleanup EXIT

run_step() {
    local description="$1"
    shift
    # Ensure provider source is injected into .crn files in the current directory
    prepare_work_dir "$(pwd)"
    printf "  %-50s" "$description"
    if "$@" > /dev/null 2>&1; then
        echo "OK"
        TEST_PASSED=$((TEST_PASSED + 1))
    else
        echo "FAIL"
        TEST_FAILED=$((TEST_FAILED + 1))
    fi
}

assert_state_value() {
    local description="$1"
    local jq_query="$2"
    local expected="$3"
    local work_dir="$4"

    printf "  %-50s" "$description"
    local actual
    actual=$(jq -r "$jq_query" "$work_dir/carina.state.json" 2>/dev/null)
    if [ "$actual" = "$expected" ]; then
        echo "OK"
        TEST_PASSED=$((TEST_PASSED + 1))
    else
        echo "FAIL (expected '$expected', got '$actual')"
        TEST_FAILED=$((TEST_FAILED + 1))
    fi
}

assert_state_resource_count() {
    local description="$1"
    local expected="$2"
    local work_dir="$3"

    printf "  %-50s" "$description"
    local actual
    actual=$(jq '.resources | length' "$work_dir/carina.state.json" 2>/dev/null)
    if [ "$actual" = "$expected" ]; then
        echo "OK"
        TEST_PASSED=$((TEST_PASSED + 1))
    else
        echo "FAIL (expected '$expected', got '$actual')"
        TEST_FAILED=$((TEST_FAILED + 1))
    fi
}

# Print test results and exit with appropriate code
finish_test() {
    echo ""
    echo "  Results: $TEST_PASSED passed, $TEST_FAILED failed"
    if [ "$TEST_FAILED" -gt 0 ]; then
        exit 1
    fi
}
