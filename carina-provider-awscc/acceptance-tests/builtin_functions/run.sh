#!/bin/bash
# Acceptance tests for built-in functions
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
FILTER="${1:-}"

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "Building carina..."
    cargo build --quiet 2>/dev/null || cargo build
fi

TOTAL_PASSED=0
TOTAL_FAILED=0

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
        TOTAL_PASSED=$((TOTAL_PASSED + 1))
    else
        echo "FAIL"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
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
        TOTAL_PASSED=$((TOTAL_PASSED + 1))
    else
        echo "FAIL (expected '$expected', got '$actual')"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
    fi
}

echo "builtin_functions acceptance tests"
echo "════════════════════════════════════════"

# ─── Test: join() ───
if [ -z "$FILTER" ] || echo "join" | grep -q "$FILTER"; then
    echo ""
    echo "Test: join() function"
    echo ""

    WORK_DIR=$(mktemp -d)
    ACTIVE_WORK_DIR="$WORK_DIR"
    cp "$SCRIPT_DIR/join.crn" "$WORK_DIR/main.crn"

    cd "$WORK_DIR"

    run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
    run_step "step2: plan-verify" "$CARINA_BIN" plan .

    # Verify the join() result in state
    assert_state_value \
        "assert: tag Name = 'web-test-vpc'" \
        '.resources[0].attributes.tags.Name' \
        'web-test-vpc' \
        "$WORK_DIR"

    # Cleanup
    echo "  Cleanup: destroying resources..."
    "$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
    rm -rf "$WORK_DIR"
    ACTIVE_WORK_DIR=""
fi

# ─── Test: cidr_subnet() ───
if [ -z "$FILTER" ] || echo "cidr_subnet" | grep -q "$FILTER"; then
    echo ""
    echo "Test: cidr_subnet() function"
    echo ""

    WORK_DIR=$(mktemp -d)
    ACTIVE_WORK_DIR="$WORK_DIR"
    cp "$SCRIPT_DIR/cidr_subnet.crn" "$WORK_DIR/main.crn"

    cd "$WORK_DIR"

    run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
    run_step "step2: plan-verify" "$CARINA_BIN" plan .

    # Verify the cidr_subnet() result in state - subnet should have cidr_block = "10.0.1.0/24"
    assert_state_value \
        "assert: subnet cidr_block = '10.0.1.0/24'" \
        '.resources[] | select(.resource_type == "ec2.subnet") | .attributes.cidr_block' \
        '10.0.1.0/24' \
        "$WORK_DIR"

    # Cleanup
    echo "  Cleanup: destroying resources..."
    "$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
    rm -rf "$WORK_DIR"
    ACTIVE_WORK_DIR=""
fi

echo ""
echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ "$TOTAL_FAILED" -gt 0 ]; then
    exit 1
fi
