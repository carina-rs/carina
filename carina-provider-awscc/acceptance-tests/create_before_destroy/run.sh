#!/bin/bash
# Multi-step acceptance test for create_before_destroy with temporary name generation
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh
#
# This test verifies:
#   1. Create initial IAM role with fixed name and path="/"
#   2. Change path (create-only) with create_before_destroy → triggers replacement
#      with automatic temporary name for role_name
#   3. Plan-verify after replacement is idempotent
#   4. Destroy all resources

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "Building carina..."
    cargo build --quiet 2>/dev/null || cargo build
fi

WORK_DIR=$(mktemp -d)
trap "rm -rf $WORK_DIR" EXIT

STEP1="$SCRIPT_DIR/iam_role_step1.crn"
STEP2="$SCRIPT_DIR/iam_role_step2.crn"

PASSED=0
FAILED=0

run_step() {
    local description="$1"
    local command="$2"
    local crn_file="$3"
    local extra_args="${4:-}"

    printf "  %-55s " "$description"

    local output
    if output=$(cd "$WORK_DIR" && "$CARINA_BIN" $command $extra_args "$crn_file" 2>&1); then
        echo "OK"
        PASSED=$((PASSED + 1))
        return 0
    else
        echo "FAIL"
        echo "  ERROR: $output"
        FAILED=$((FAILED + 1))
        return 1
    fi
}

run_plan_verify() {
    local description="$1"
    local crn_file="$2"

    printf "  %-55s " "$description"

    local output
    local rc
    output=$(cd "$WORK_DIR" && "$CARINA_BIN" plan --detailed-exitcode "$crn_file" 2>&1) || rc=$?
    rc=${rc:-0}

    if [ $rc -eq 2 ]; then
        echo "FAIL"
        echo "  ERROR: Post-apply plan detected changes (not idempotent):"
        echo "  $output"
        FAILED=$((FAILED + 1))
        return 1
    elif [ $rc -ne 0 ]; then
        echo "FAIL"
        echo "  ERROR: $output"
        FAILED=$((FAILED + 1))
        return 1
    fi

    echo "OK"
    PASSED=$((PASSED + 1))
    return 0
}

echo "create_before_destroy temporary name test (IAM Role)"
echo ""

# Step 1: Apply initial config
if ! run_step "step1: apply initial (path=/)" "apply" "$STEP1" "--auto-approve"; then
    echo ""
    echo "Results: $PASSED passed, $FAILED failed"
    exit 1
fi

# Step 1b: Plan-verify initial state
if ! run_plan_verify "step1: plan-verify initial" "$STEP1"; then
    cd "$WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$STEP1" 2>&1 || true
    echo ""
    echo "Results: $PASSED passed, $FAILED failed"
    exit 1
fi

# Step 2: Apply modified config (path changes → replacement with temp name)
if ! run_step "step2: apply replace (path=/carina/, cbd)" "apply" "$STEP2" "--auto-approve"; then
    # Try to destroy with both configs to clean up
    cd "$WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$STEP2" 2>&1 || true
    cd "$WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$STEP1" 2>&1 || true
    echo ""
    echo "Results: $PASSED passed, $FAILED failed"
    exit 1
fi

# Step 3: Plan-verify after replacement
if ! run_plan_verify "step3: plan-verify after replace" "$STEP2"; then
    cd "$WORK_DIR" && "$CARINA_BIN" destroy --auto-approve "$STEP2" 2>&1 || true
    echo ""
    echo "Results: $PASSED passed, $FAILED failed"
    exit 1
fi

# Step 4: Destroy
run_step "step4: destroy" "destroy" "$STEP2" "--auto-approve"

echo ""
echo "Results: $PASSED passed, $FAILED failed"

if [ $FAILED -gt 0 ]; then
    exit 1
fi
