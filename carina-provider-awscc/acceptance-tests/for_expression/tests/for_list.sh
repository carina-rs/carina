#!/bin/bash
# Test: for expression over a list with index
source "$(dirname "$0")/_helpers.sh"

echo "Test: for expression over list"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/for_list.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 2 resources created" "2" "$WORK_DIR"

# Verify each VPC has the correct tags
# Resources are vpcs[0] and vpcs[1] with cidr_subnet("10.0.0.0/8", 8, 0) = 10.0.0.0/16 and cidr_subnet("10.0.0.0/8", 8, 1) = 10.1.0.0/16
assert_state_value "assert: vpcs[0] Name = 'for-list-test-dev'" \
    '[.resources[] | select(.binding == "vpcs[0]")] | .[0].attributes.tags.Name' \
    'for-list-test-dev' "$WORK_DIR"

assert_state_value "assert: vpcs[0] cidr_block = '10.0.0.0/16'" \
    '[.resources[] | select(.binding == "vpcs[0]")] | .[0].attributes.cidr_block' \
    '10.0.0.0/16' "$WORK_DIR"

assert_state_value "assert: vpcs[1] Name = 'for-list-test-stg'" \
    '[.resources[] | select(.binding == "vpcs[1]")] | .[0].attributes.tags.Name' \
    'for-list-test-stg' "$WORK_DIR"

assert_state_value "assert: vpcs[1] cidr_block = '10.1.0.0/16'" \
    '[.resources[] | select(.binding == "vpcs[1]")] | .[0].attributes.cidr_block' \
    '10.1.0.0/16' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
