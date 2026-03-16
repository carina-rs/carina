#!/bin/bash
# Multi-step acceptance tests for in-place updates
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]
#
# Tests:
#   logs_log_group                     - Change retention_in_days (7 -> 14)
#   s3_bucket                          - Toggle versioning (Enabled -> Disabled)
#   ec2_vpc                            - Toggle enable_dns_hostnames (false -> true)
#   ec2_subnet                         - Toggle map_public_ip_on_launch (false -> true)
#   iam_role                           - Change max_session_duration (3600 -> 7200)
#   ec2_transit_gateway                - Change description
#   ec2_security_group                 - Add ingress rule
#   ec2_eip                            - Update tags
#   ec2_route_table                    - Update tags
#   ec2_flow_log                       - Update tags
#   ec2_nat_gateway                    - Update tags
#   ec2_egress_only_internet_gateway   - Update tags
#   ec2_vpc_endpoint                   - Add policy_document
#   ec2_vpc_peering_connection         - Update tags
#   ec2_transit_gateway_attachment      - Update tags
#   ec2_route                          - Change route target (IGW -> NAT GW)
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

# Test 3: EC2 VPC - toggle enable_dns_hostnames (false -> true)
run_test "ec2_vpc" \
    "$SCRIPT_DIR/ec2_vpc_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_step2.crn" \
    "Test 3: EC2 VPC (enable_dns_hostnames false -> true)"

# Test 4: EC2 Subnet - toggle map_public_ip_on_launch (false -> true)
run_test "ec2_subnet" \
    "$SCRIPT_DIR/ec2_subnet_step1.crn" \
    "$SCRIPT_DIR/ec2_subnet_step2.crn" \
    "Test 4: EC2 Subnet (map_public_ip_on_launch false -> true)"

# Test 5: IAM Role - change max_session_duration (3600 -> 7200)
run_test "iam_role" \
    "$SCRIPT_DIR/iam_role_step1.crn" \
    "$SCRIPT_DIR/iam_role_step2.crn" \
    "Test 5: IAM Role (max_session_duration 3600 -> 7200)"

# Test 6: EC2 Transit Gateway - change description
run_test "ec2_transit_gateway" \
    "$SCRIPT_DIR/ec2_transit_gateway_step1.crn" \
    "$SCRIPT_DIR/ec2_transit_gateway_step2.crn" \
    "Test 6: EC2 Transit Gateway (description update)"

# Test 7: EC2 Security Group - add ingress rule
run_test "ec2_security_group" \
    "$SCRIPT_DIR/ec2_security_group_step1.crn" \
    "$SCRIPT_DIR/ec2_security_group_step2.crn" \
    "Test 7: EC2 Security Group (add HTTPS ingress rule)"

# Test 8: EC2 EIP - update tags
run_test "ec2_eip" \
    "$SCRIPT_DIR/ec2_eip_step1.crn" \
    "$SCRIPT_DIR/ec2_eip_step2.crn" \
    "Test 8: EC2 EIP (tags update)"

# Test 9: EC2 Route Table - update tags
run_test "ec2_route_table" \
    "$SCRIPT_DIR/ec2_route_table_step1.crn" \
    "$SCRIPT_DIR/ec2_route_table_step2.crn" \
    "Test 9: EC2 Route Table (tags update)"

# Test 10: EC2 Flow Log - update tags
run_test "ec2_flow_log" \
    "$SCRIPT_DIR/ec2_flow_log_step1.crn" \
    "$SCRIPT_DIR/ec2_flow_log_step2.crn" \
    "Test 10: EC2 Flow Log (tags update)"

# Test 11: EC2 NAT Gateway - update tags
run_test "ec2_nat_gateway" \
    "$SCRIPT_DIR/ec2_nat_gateway_step1.crn" \
    "$SCRIPT_DIR/ec2_nat_gateway_step2.crn" \
    "Test 11: EC2 NAT Gateway (tags update)"

# Test 12: EC2 Egress Only Internet Gateway - update tags
run_test "ec2_egress_only_internet_gateway" \
    "$SCRIPT_DIR/ec2_egress_only_internet_gateway_step1.crn" \
    "$SCRIPT_DIR/ec2_egress_only_internet_gateway_step2.crn" \
    "Test 12: EC2 Egress Only Internet Gateway (tags update)"

# Test 13: EC2 VPC Endpoint - add policy_document
run_test "ec2_vpc_endpoint" \
    "$SCRIPT_DIR/ec2_vpc_endpoint_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_endpoint_step2.crn" \
    "Test 13: EC2 VPC Endpoint (add policy_document)"

# Test 14: EC2 VPC Peering Connection - update tags
run_test "ec2_vpc_peering_connection" \
    "$SCRIPT_DIR/ec2_vpc_peering_connection_step1.crn" \
    "$SCRIPT_DIR/ec2_vpc_peering_connection_step2.crn" \
    "Test 14: EC2 VPC Peering Connection (tags update)"

# Test 15: EC2 Transit Gateway Attachment - update tags
run_test "ec2_transit_gateway_attachment" \
    "$SCRIPT_DIR/ec2_transit_gateway_attachment_step1.crn" \
    "$SCRIPT_DIR/ec2_transit_gateway_attachment_step2.crn" \
    "Test 15: EC2 Transit Gateway Attachment (tags update)"

# Test 16: EC2 Route - change route target (IGW -> NAT GW)
run_test "ec2_route" \
    "$SCRIPT_DIR/ec2_route_step1.crn" \
    "$SCRIPT_DIR/ec2_route_step2.crn" \
    "Test 16: EC2 Route (target IGW -> NAT GW)"

echo "════════════════════════════════════════"
echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ $TOTAL_FAILED -gt 0 ]; then
    exit 1
fi
