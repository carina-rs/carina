#!/bin/bash
# Test: resource-generating user-defined function called multiple times
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: resource-generating user function"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/resource_fn.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 2 resources created" "2" "$WORK_DIR"

# Verify dev VPC
assert_state_value "assert: dev_vpc cidr_block" \
    '[.resources[] | select(.binding == "dev_vpc")] | .[0].attributes.cidr_block' \
    '10.0.0.0/16' "$WORK_DIR"

assert_state_value "assert: dev_vpc Name tag" \
    '[.resources[] | select(.binding == "dev_vpc")] | .[0].attributes.tags.Name' \
    'dev-vpc' "$WORK_DIR"

assert_state_value "assert: dev_vpc Environment tag" \
    '[.resources[] | select(.binding == "dev_vpc")] | .[0].attributes.tags.Environment' \
    'dev' "$WORK_DIR"

# Verify stg VPC
assert_state_value "assert: stg_vpc cidr_block" \
    '[.resources[] | select(.binding == "stg_vpc")] | .[0].attributes.cidr_block' \
    '10.1.0.0/16' "$WORK_DIR"

assert_state_value "assert: stg_vpc Name tag" \
    '[.resources[] | select(.binding == "stg_vpc")] | .[0].attributes.tags.Name' \
    'stg-vpc' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
