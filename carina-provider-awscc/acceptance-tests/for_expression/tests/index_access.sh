#!/bin/bash
# Test: index access on for expression results (vpcs[0].vpc_id used as subnet dependency)
source "$(dirname "$0")/_helpers.sh"

echo "Test: index access on for expression results"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/index_access.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

# 2 VPCs from for + 1 subnet = 3 resources
assert_state_resource_count "assert: 3 resources created" "3" "$WORK_DIR"

# Subnet should exist and reference vpcs[0]'s VPC ID
assert_state_value "assert: subnet Name tag" \
    '[.resources[] | select(.resource_type == "ec2.subnet")] | .[0].attributes.tags.Name' \
    'index-access-test-subnet' "$WORK_DIR"

# Subnet's vpc_id should match vpcs[0]'s vpc_id
SUBNET_VPC_ID=$(jq -r '[.resources[] | select(.resource_type == "ec2.subnet")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")
VPC0_VPC_ID=$(jq -r '[.resources[] | select(.binding == "vpcs[0]")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")

printf "  %-50s" "assert: subnet vpc_id matches vpcs[0].vpc_id"
if [ "$SUBNET_VPC_ID" = "$VPC0_VPC_ID" ] && [ -n "$SUBNET_VPC_ID" ] && [ "$SUBNET_VPC_ID" != "null" ]; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (subnet=$SUBNET_VPC_ID, vpcs[0]=$VPC0_VPC_ID)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
