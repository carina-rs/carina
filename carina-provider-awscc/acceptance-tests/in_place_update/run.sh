#!/bin/bash
# Multi-step acceptance tests for in-place updates
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   logs_log_group - Change retention_in_days (7 -> 14)
#   s3_bucket      - Toggle versioning (Enabled -> Disabled)
#
# Filter (optional): substring to match test names (e.g. "logs_log_group", "s3_bucket")

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

    # Step 2: Apply modified config (in-place update)
    if ! run_step "$work_dir" "step2: apply in-place update" "apply" "$step2" "--auto-approve"; then
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

echo "in_place_update multi-step acceptance tests"
echo "════════════════════════════════════════"
echo ""

# Test 1: CloudWatch Logs Log Group - change retention_in_days (7 -> 14)
run_test "logs_log_group" \
    "$SCRIPT_DIR/logs_log_group_step1.crn" \
    "$SCRIPT_DIR/logs_log_group_step2.crn" \
    "Test 1: Logs Log Group (retention_in_days 7 -> 14)"

# Test 2: S3 Bucket - toggle versioning (Enabled -> Disabled)
run_test "s3_bucket" \
    "$SCRIPT_DIR/s3_bucket_step1.crn" \
    "$SCRIPT_DIR/s3_bucket_step2.crn" \
    "Test 2: S3 Bucket (versioning Enabled -> Disabled)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
