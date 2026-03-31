#!/bin/bash
# Multi-step acceptance tests for in-place updates
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   ec2_vpc            - Toggle enable_dns_hostnames (false -> true)
#   ec2_route_table    - Update tags
#   ec2_security_group - Update tags
#   ec2_subnet         - Toggle map_public_ip_on_launch (false -> true)
#   ec2_route          - Change route target (IGW -> NAT Gateway)
#   s3_bucket          - Toggle versioning (Enabled -> Suspended)
#   s3_bucket_acl      - ACL/object_ownership transition
#
# Filter (optional): substring to match test names (e.g. "ec2_vpc", "s3_bucket")

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
FILTER="${1:-}"

source "$SCRIPT_DIR/../shared/_helpers.sh"

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

# Extract all resource identifiers from carina.state.json as a sorted newline-separated string.
# Args: work_dir
# Outputs: sorted identifiers (one per line), or empty if none found
get_identifiers() {
    local work_dir="$1"
    jq -r '.resources[].identifier // empty' "$work_dir/carina.state.json" 2>/dev/null | sort || true
}

# Assert that two identifier sets match the expected relationship
# Args: description ids_after_step1 ids_after_step2 expected("equal"|"different")
assert_identifiers() {
    local description="$1"
    local ids1="$2"
    local ids2="$3"
    local expected="$4"

    printf "  %-55s " "$description"

    if [ -z "$ids1" ] || [ -z "$ids2" ]; then
        echo "FAIL"
        echo "  ERROR: Could not extract identifiers (step1='$ids1', step2='$ids2')"
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
        return 1
    fi

    if [ "$expected" = "equal" ]; then
        if [ "$ids1" = "$ids2" ]; then
            echo "OK"
            TOTAL_PASSED=$((TOTAL_PASSED + 1))
            return 0
        else
            echo "FAIL"
            echo "  ERROR: Identifiers changed (expected same):"
            echo "    before: $ids1"
            echo "    after:  $ids2"
            TOTAL_FAILED=$((TOTAL_FAILED + 1))
            return 1
        fi
    else
        if [ "$ids1" != "$ids2" ]; then
            echo "OK"
            TOTAL_PASSED=$((TOTAL_PASSED + 1))
            return 0
        else
            echo "FAIL"
            echo "  ERROR: Identifiers unchanged (expected different): $ids1"
            TOTAL_FAILED=$((TOTAL_FAILED + 1))
            return 1
        fi
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

    # Inject provider source into .crn files
    step1=$(inject_provider_source "$step1")
    step2=$(inject_provider_source "$step2")

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

    # Capture identifiers after step 1
    local ids_after_step1
    ids_after_step1=$(get_identifiers "$work_dir")

    # Step 2: Apply modified config (in-place update)
    if ! run_step "$work_dir" "step2: apply in-place update" "apply" "$step2" "--auto-approve"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Capture identifiers after step 2
    local ids_after_step2
    ids_after_step2=$(get_identifiers "$work_dir")

    # Assert identifiers preserved (in-place update should NOT replace any resource)
    if ! assert_identifiers "assert: identifiers preserved after update" "$ids_after_step1" "$ids_after_step2" "equal"; then
        cleanup "$work_dir" "$step2" "$step1"
        rm -rf "$work_dir"
        rm -f "$step1" "$step2"
        ACTIVE_WORK_DIR=""
        return 1
    fi

    # Step 3: Plan-verify after update
    if ! run_plan_verify "$work_dir" "step3: plan-verify after update" "$step2"; then
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

echo "in_place_update multi-step acceptance tests (AWS)"
echo "════════════════════════════════════════"
echo ""

# Test 1: EC2 VPC - toggle enable_dns_hostnames (false -> true)
run_test "ec2_vpc" \
    "$SCRIPT_DIR/ec2_vpc_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_step2.crn" \
    "Test 1: EC2 VPC (enable_dns_hostnames false -> true)"

# Test 2: EC2 Route Table - update tags
run_test "ec2_route_table" \
    "$SCRIPT_DIR/ec2_route_table_step1.crn" \
    "$SCRIPT_DIR/ec2_route_table_step2.crn" \
    "Test 2: EC2 Route Table (tags update)"

# Test 3: EC2 Security Group - update tags
run_test "ec2_security_group" \
    "$SCRIPT_DIR/ec2_security_group_step1.crn" \
    "$SCRIPT_DIR/ec2_security_group_step2.crn" \
    "Test 3: EC2 Security Group (tags update)"

# Test 4: EC2 Subnet - toggle map_public_ip_on_launch (false -> true)
run_test "ec2_subnet" \
    "$SCRIPT_DIR/ec2_subnet_step1.crn" \
    "$SCRIPT_DIR/ec2_subnet_step2.crn" \
    "Test 4: EC2 Subnet (map_public_ip_on_launch false -> true)"

# Test 5: EC2 Route - change route target (IGW -> NAT Gateway)
run_test "ec2_route" \
    "$SCRIPT_DIR/ec2_route_step1.crn" \
    "$SCRIPT_DIR/ec2_route_step2.crn" \
    "Test 5: EC2 Route (gateway_id -> nat_gateway_id)"

# Test 6: S3 Bucket - toggle versioning (Enabled -> Suspended)
run_test "s3_bucket" \
    "$SCRIPT_DIR/s3_bucket_step1.crn" \
    "$SCRIPT_DIR/s3_bucket_step2.crn" \
    "Test 6: S3 Bucket (versioning Enabled -> Suspended)"

# Test 7: S3 Bucket ACL - ACL/object_ownership transition
run_test "s3_bucket_acl" \
    "$SCRIPT_DIR/s3_bucket_acl_step1.crn" \
    "$SCRIPT_DIR/s3_bucket_acl_step2.crn" \
    "Test 7: S3 Bucket ACL (ACL private + BucketOwnerPreferred -> BucketOwnerEnforced)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
