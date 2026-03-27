#!/bin/bash
# Test: module attributes access (net.vpc_id used to create dependent route table)
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: module attributes access"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/attributes_access.crn" "$WORK_DIR/main.crn"
cp -r "$SCRIPT_DIR/modules" "$WORK_DIR/modules"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

# Module produces vpc + subnet + virtual resource, plus route_table = 4 resources
# (virtual resources are not in state)
assert_state_resource_count "assert: 3 resources created" "3" "$WORK_DIR"

# Verify route table exists with correct tag
assert_state_value "assert: route_table Name tag" \
    '[.resources[] | select(.resource_type == "ec2.route_table")] | .[0].attributes.tags.Name' \
    'module-attr-test' "$WORK_DIR"

# Verify route table's vpc_id matches the module's vpc
RT_VPC_ID=$(jq -r '[.resources[] | select(.resource_type == "ec2.route_table")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")
VPC_VPC_ID=$(jq -r '[.resources[] | select(.resource_type == "ec2.vpc")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")

printf "  %-50s" "assert: route_table vpc_id matches module vpc"
if [ "$RT_VPC_ID" = "$VPC_VPC_ID" ] && [ -n "$RT_VPC_ID" ] && [ "$RT_VPC_ID" != "null" ]; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (rt=$RT_VPC_ID, vpc=$VPC_VPC_ID)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
