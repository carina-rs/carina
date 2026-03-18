#!/bin/bash
# Multi-step acceptance tests for create_before_destroy
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   iam_role                      - Replacement with temporary name (name_attribute + other create-only)
#   ec2_vpc                       - Replacement without temporary name (no name_attribute)
#   ec2_transit_gateway_attachment - Replacement with dependent resource rewiring (subnet_ids change)
#
# Filter (optional): substring to match test names (e.g. "iam_role", "ec2_vpc", "ec2_transit")

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
# Returns 0 if at least one destroy succeeded, 1 if ALL failed
cleanup() {
    local work_dir="$1"
    local step2="$2"
    local step1="$3"
    local any_success=false

    # Disable set -e to ensure all destroy attempts run
    set +e
    echo "  Cleanup: destroying resources..."
    if cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step2" 2>&1; then
        any_success=true
    fi
    if cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step1" 2>&1; then
        any_success=true
    fi
    # Retry: resources may have dependencies that prevent deletion on first pass
    if cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step2" 2>&1; then
        any_success=true
    fi
    if cd "$work_dir" && "$CARINA_BIN" destroy --auto-approve "$step1" 2>&1; then
        any_success=true
    fi
    set -e

    if [ "$any_success" = false ]; then
        return 1
    fi
    return 0
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

    # Register for signal cleanup
    ACTIVE_WORK_DIR="$work_dir"
    ACTIVE_STEP1="$step1"
    ACTIVE_STEP2="$step2"

    echo "$desc"
    echo ""

    # Step 1: Apply initial config
    if ! run_step "$work_dir" "step1: apply initial" "apply" "$step1" "--auto-approve"; then
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

    # Step 2: Apply modified config (triggers create_before_destroy replacement)
    if ! run_step "$work_dir" "step2: apply replace (create_before_destroy)" "apply" "$step2" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 3: Plan-verify after replacement
    if ! run_plan_verify "$work_dir" "step3: plan-verify after replace" "$step2"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 4: Destroy (use cleanup to try both configs and retry)
    if ! cleanup "$work_dir" "$step2" "$step1"; then
        echo "  WARNING: All destroy attempts failed. Preserving work dir for debugging:"
        echo "    $work_dir"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        ACTIVE_WORK_DIR=""
        echo ""
        return 1
    fi

    rm -rf "$work_dir"
    ACTIVE_WORK_DIR=""
    echo ""
}

echo "create_before_destroy multi-step acceptance tests"
echo "════════════════════════════════════════"
echo ""

# Test 1: IAM Role - replacement WITH temporary name
# name_attribute=role_name, path is another create-only property
run_test "iam_role" \
    "$SCRIPT_DIR/iam_role_step1.crn" \
    "$SCRIPT_DIR/iam_role_step2.crn" \
    "Test 1: IAM Role (temporary name generation, can_rename=false)"

# Test 2: EC2 VPC - replacement WITHOUT temporary name
# No name_attribute, cidr_block is create-only
run_test "ec2_vpc" \
    "$SCRIPT_DIR/ec2_vpc_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_step2.crn" \
    "Test 2: EC2 VPC (no name_attribute, no temporary name)"

# Test 3: EC2 Transit Gateway Attachment - replacement with dependent resource rewiring
# subnet_ids is create-only; changing it forces replacement while VPC/TGW refs must be rewired
run_test "ec2_transit_gateway_attachment" \
    "$SCRIPT_DIR/ec2_transit_gateway_attachment_step1.crn" \
    "$SCRIPT_DIR/ec2_transit_gateway_attachment_step2.crn" \
    "Test 3: EC2 Transit Gateway Attachment (dependent resource rewiring)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
