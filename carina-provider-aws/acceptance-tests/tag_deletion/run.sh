#!/bin/bash
# Multi-step acceptance tests for tag deletion
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   ec2_vpc              - Remove a tag from VPC (EC2 delete_tags API)
#   s3_bucket            - Remove a tag from S3 bucket (put_bucket_tagging API)
#   ec2_vpc_all_tags     - Remove entire tags block from VPC (issue #447)
#   s3_bucket_all_tags   - Remove entire tags block from S3 bucket (issue #447)
#
# Filter (optional): substring to match test names (e.g. "ec2_vpc", "s3_bucket")

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

# Track active work dir for signal cleanup
ACTIVE_WORK_DIR=""
ACTIVE_STEP1=""
ACTIVE_STEP2=""

signal_cleanup() {
    if [ -n "$ACTIVE_WORK_DIR" ] && [ -d "$ACTIVE_WORK_DIR" ]; then
        set +e
        echo ""
        echo "Interrupted. Cleaning up resources..."
        cd "$ACTIVE_WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$ACTIVE_STEP2" 2>&1
        cd "$ACTIVE_WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$ACTIVE_STEP1" 2>&1
        cd "$ACTIVE_WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$ACTIVE_STEP2" 2>&1
        cd "$ACTIVE_WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$ACTIVE_STEP1" 2>&1
        rm -rf "$ACTIVE_WORK_DIR"
        ACTIVE_WORK_DIR=""
    fi
    exit 1
}

trap signal_cleanup INT TERM

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

# Cleanup helper: try to destroy with both step configs, then retry
cleanup() {
    local work_dir="$1"
    local step2="$2"
    local step1="$3"

    # Disable set -e to ensure all destroy attempts run
    set +e
    echo "  Cleanup: destroying resources..."
    cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step2" 2>&1
    cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step1" 2>&1
    # Retry: resources may have dependencies that prevent deletion on first pass
    cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step2" 2>&1
    cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step1" 2>&1
    set -e
}

# Run a single multi-step tag deletion test
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

    # Register for signal cleanup
    ACTIVE_WORK_DIR="$work_dir"
    ACTIVE_STEP1="$step1"
    ACTIVE_STEP2="$step2"

    echo "$desc"
    echo ""

    # Step 1: Apply initial config (resource with two tags)
    if ! run_step "$work_dir" "step1: apply initial (two tags)" "apply" "$step1" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 1b: Plan-verify initial state
    if ! run_plan_verify "$work_dir" "step1: plan-verify initial" "$step1"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 2: Apply modified config (one tag removed)
    if ! run_step "$work_dir" "step2: apply tag removal" "apply" "$step2" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 3: Plan-verify after tag removal (must be idempotent)
    if ! run_plan_verify "$work_dir" "step3: plan-verify after tag removal" "$step2"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 4: Destroy (use cleanup to try both configs and retry)
    cleanup "$work_dir" "$step2" "$step1"

    rm -rf "$work_dir"
    ACTIVE_WORK_DIR=""
    echo ""
}

echo "tag_deletion multi-step acceptance tests"
echo "════════════════════════════════════════"
echo ""

# Test 1: EC2 VPC - tag removal via delete_tags API
run_test "ec2_vpc" \
    "$SCRIPT_DIR/ec2_vpc_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_step2.crn" \
    "Test 1: EC2 VPC (delete_tags for removed tag key)"

# Test 2: S3 Bucket - tag removal via put_bucket_tagging API
run_test "s3_bucket" \
    "$SCRIPT_DIR/s3_bucket_step1.crn" \
    "$SCRIPT_DIR/s3_bucket_step2.crn" \
    "Test 2: S3 Bucket (put_bucket_tagging replaces tag set)"

# Test 3: EC2 VPC - entire tags block removal (issue #447)
run_test "ec2_vpc_all_tags" \
    "$SCRIPT_DIR/ec2_vpc_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_step3.crn" \
    "Test 3: EC2 VPC (remove entire tags block)"

# Test 4: S3 Bucket - entire tags block removal (issue #447)
run_test "s3_bucket_all_tags" \
    "$SCRIPT_DIR/s3_bucket_step1.crn" \
    "$SCRIPT_DIR/s3_bucket_step3.crn" \
    "Test 4: S3 Bucket (remove entire tags block)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
