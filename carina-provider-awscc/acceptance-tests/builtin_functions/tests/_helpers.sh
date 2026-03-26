#!/bin/bash
# Common helpers for builtin function acceptance tests

set -e

HELPERS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_DIR="$(cd "$HELPERS_DIR/.." && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "Building carina..."
    cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml" --quiet 2>/dev/null \
        || cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml"
fi

TEST_PASSED=0
TEST_FAILED=0

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

# Print test results and exit with appropriate code
finish_test() {
    echo ""
    echo "  Results: $TEST_PASSED passed, $TEST_FAILED failed"
    if [ "$TEST_FAILED" -gt 0 ]; then
        exit 1
    fi
}
