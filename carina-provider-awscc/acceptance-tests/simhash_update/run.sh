#!/bin/bash
# Multi-step acceptance tests for SimHash reconciliation
#
# Verifies that anonymous resources (with no create-only property values)
# are correctly reconciled via SimHash Hamming distance when attributes change.
# The attribute change should trigger an in-place Update, not Delete+Create.
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   ec2_eip              - Case A: schema has create-only props, but none set by user
#   ec2_internet_gateway - Case B: schema has no create-only props at all
#
# Filter (optional): substring to match test names

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

run_step() {
    local work_dir="$1"
    local description="$2"
    local command="$3"
    local crn_file="$4"
    local extra_args="${5:-}"

    printf "  %-55s " "$description"

    local output
    if output=$(cd "$work_dir" && "$CARINA_BIN" $command $extra_args "$crn_file" 2>&1); then
        echo "OK"
        TOTAL_PASSED=$((TOTAL_PASSED + 1))
        return 0
    else
        echo "FAIL"
        echo "  ERROR: $output"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        return 1
    fi
}

run_plan_verify() {
    local work_dir="$1"
    local description="$2"
    local crn_file="$3"

    printf "  %-55s " "$description"

    local output
    local rc
    output=$(cd "$work_dir" && "$CARINA_BIN" plan --detailed-exitcode "$crn_file" 2>&1) || rc=$?
    rc=${rc:-0}

    if [ $rc -eq 2 ]; then
        echo "FAIL"
        echo "  ERROR: Post-apply plan detected changes (not idempotent):"
        echo "  $output"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        return 1
    elif [ $rc -ne 0 ]; then
        echo "FAIL"
        echo "  ERROR: $output"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        return 1
    fi

    echo "OK"
    TOTAL_PASSED=$((TOTAL_PASSED + 1))
    return 0
}

# Cleanup helper: try to destroy with both step configs
cleanup() {
    local work_dir="$1"
    local step2="$2"
    local step1="$3"
    cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step2" 2>&1 || true
    cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step1" 2>&1 || true
}

# Run a single multi-step test
# Args: test_name step1_crn step2_crn description
run_test() {
    local test_name="$1"
    local step1="$2"
    local step2="$3"
    local desc="$4"

    # Apply filter
    if [ -n "$FILTER" ] && [[ "$test_name" != *"$FILTER"* ]]; then
        return 0
    fi

    local work_dir
    work_dir=$(mktemp -d)

    echo "$desc"
    echo ""

    # Step 1: Apply initial config
    if ! run_step "$work_dir" "step1: apply initial" "apply" "$step1" "--auto-approve"; then
        rm -rf "$work_dir"
        return 1
    fi

    # Step 1b: Plan-verify initial state
    if ! run_plan_verify "$work_dir" "step1: plan-verify initial" "$step1"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        return 1
    fi

    # Step 2: Apply modified config (SimHash reconciliation should match)
    if ! run_step "$work_dir" "step2: apply update (simhash reconcile)" "apply" "$step2" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        return 1
    fi

    # Step 3: Plan-verify after update
    if ! run_plan_verify "$work_dir" "step3: plan-verify after update" "$step2"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        return 1
    fi

    # Step 4: Destroy
    run_step "$work_dir" "step4: destroy" "destroy" "$step2" "--auto-approve"

    rm -rf "$work_dir"
    echo ""
}

echo "simhash_update multi-step acceptance tests"
echo "════════════════════════════════════════"
echo ""

# Test 1: EC2 EIP - Case A: schema has create-only props, but user didn't set any
# Change tag Environment (acceptance-test -> staging)
run_test "ec2_eip" \
    "$SCRIPT_DIR/ec2_eip_step1.crn" \
    "$SCRIPT_DIR/ec2_eip_step2.crn" \
    "Test 1: EC2 EIP (tag update, Case A: create-only props exist but not set)"

# Test 2: EC2 Internet Gateway - Case B: schema has no create-only props at all
# Change tag Environment (acceptance-test -> staging)
run_test "ec2_internet_gateway" \
    "$SCRIPT_DIR/ec2_internet_gateway_step1.crn" \
    "$SCRIPT_DIR/ec2_internet_gateway_step2.crn" \
    "Test 2: EC2 Internet Gateway (tag update, Case B: no create-only props)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
