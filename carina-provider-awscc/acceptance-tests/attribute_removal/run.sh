#!/bin/bash
# Multi-step acceptance tests for AWSCC attribute removal via CloudControl "remove" patch
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   ec2_vpc_endpoint - Remove policy_document from VPC endpoint
#   logs_log_group   - Remove retention_in_days from log group
#
# Filter (optional): substring to match test names (e.g. "ec2_vpc_endpoint")

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
FILTER="${1:-}"

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "Building carina..."
    cargo build --quiet 2>/dev/null || cargo build
fi

# ── Provider source injection ────────────────────────────────────────
AWSCC_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-awscc"
AWS_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-aws"

inject_provider_source() {
    local original="$1"
    local tmp_file
    tmp_file=$(mktemp "${TMPDIR:-/tmp}/carina-test-XXXXXX.crn")
    sed \
        -e '/^provider awscc {/a\
  source = "file://'"$AWSCC_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        -e '/^provider aws {/a\
  source = "file://'"$AWS_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        "$original" > "$tmp_file"
    echo "$tmp_file"
}

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

# Run a single multi-step attribute removal test
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

    # Inject provider source into .crn files
    step1=$(inject_provider_source "$step1")
    step2=$(inject_provider_source "$step2")

    # Register for signal cleanup
    ACTIVE_WORK_DIR="$work_dir"
    ACTIVE_STEP1="$step1"
    ACTIVE_STEP2="$step2"

    echo "$desc"
    echo ""

    # Step 1: Apply initial config (with policy_document)
    if ! run_step "$work_dir" "step1: apply initial (with attribute)" "apply" "$step1" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 1b: Plan-verify initial state
    if ! run_plan_verify "$work_dir" "step1: plan-verify initial" "$step1"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 2: Apply modified config (attribute removed)
    if ! run_step "$work_dir" "step2: apply attribute removal" "apply" "$step2" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 3: Plan-verify after attribute removal (must be idempotent)
    if ! run_plan_verify "$work_dir" "step3: plan-verify after attribute removal" "$step2"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 4: Destroy (use cleanup to try both configs and retry)
    if ! cleanup "$work_dir" "$step2" "$step1"; then
        echo "  WARNING: All destroy attempts failed. Preserving work dir for debugging:"
        echo "    $work_dir"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        echo ""
        return 1
    fi

    rm -rf "$work_dir"
    rm -f "$step1" "$step2"
    ACTIVE_WORK_DIR=""
    echo ""
}

echo "attribute_removal multi-step acceptance tests (AWSCC)"
echo "════════════════════════════════════════"
echo ""

# Test 1: EC2 VPC Endpoint - remove policy_document via CloudControl "remove" patch
run_test "ec2_vpc_endpoint" \
    "$SCRIPT_DIR/ec2_vpc_endpoint_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_endpoint_step2.crn" \
    "Test 1: EC2 VPC Endpoint (remove policy_document)"

# Test 2: Logs Log Group - remove retention_in_days via CloudControl "remove" patch
run_test "logs_log_group" \
    "$SCRIPT_DIR/logs_log_group_step1.crn" \
    "$SCRIPT_DIR/logs_log_group_step2.crn" \
    "Test 2: Logs Log Group (remove retention_in_days)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
