#!/bin/bash
# Test: for expression with module calls - complex case
# Uses network module (VPC+Subnet with intra-module refs) and cidr_subnet as argument
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: for expression with module calls (complex)"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/for_module_complex.crn" "$WORK_DIR/main.crn"
cp -r "$SCRIPT_DIR/modules" "$WORK_DIR/modules"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

# 2 iterations x 2 resources per module (vpc + subnet) = 4 real resources (+ 2 virtual)
assert_state_resource_count "assert: 4 resources created" "4" "$WORK_DIR"

# Verify dot-path bindings for dev
assert_state_value "assert: networks[\"dev\"].vpc exists" \
    '[.resources[] | select(.binding == "networks[\"dev\"].vpc")] | length' \
    '1' "$WORK_DIR"

assert_state_value "assert: networks[\"dev\"].subnet exists" \
    '[.resources[] | select(.binding == "networks[\"dev\"].subnet")] | length' \
    '1' "$WORK_DIR"

# Verify dot-path bindings for stg
assert_state_value "assert: networks[\"stg\"].vpc exists" \
    '[.resources[] | select(.binding == "networks[\"stg\"].vpc")] | length' \
    '1' "$WORK_DIR"

assert_state_value "assert: networks[\"stg\"].subnet exists" \
    '[.resources[] | select(.binding == "networks[\"stg\"].subnet")] | length' \
    '1' "$WORK_DIR"

# Verify VPC CIDRs
assert_state_value "assert: dev vpc cidr = '10.0.0.0/16'" \
    '[.resources[] | select(.binding == "networks[\"dev\"].vpc")] | .[0].attributes.cidr_block' \
    '10.0.0.0/16' "$WORK_DIR"

assert_state_value "assert: stg vpc cidr = '10.1.0.0/16'" \
    '[.resources[] | select(.binding == "networks[\"stg\"].vpc")] | .[0].attributes.cidr_block' \
    '10.1.0.0/16' "$WORK_DIR"

# Verify subnet CIDRs (cidr_subnet result: /16 + 8 bits + offset 1 = x.x.1.0/24)
assert_state_value "assert: dev subnet cidr = '10.0.1.0/24'" \
    '[.resources[] | select(.binding == "networks[\"dev\"].subnet")] | .[0].attributes.cidr_block' \
    '10.0.1.0/24' "$WORK_DIR"

assert_state_value "assert: stg subnet cidr = '10.1.1.0/24'" \
    '[.resources[] | select(.binding == "networks[\"stg\"].subnet")] | .[0].attributes.cidr_block' \
    '10.1.1.0/24' "$WORK_DIR"

# Verify subnet references the correct VPC (intra-module ref)
DEV_VPC_ID=$(jq -r '[.resources[] | select(.binding == "networks[\"dev\"].vpc")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")
DEV_SUBNET_VPC=$(jq -r '[.resources[] | select(.binding == "networks[\"dev\"].subnet")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")

printf "  %-50s" "assert: dev subnet refs dev vpc"
if [ "$DEV_VPC_ID" = "$DEV_SUBNET_VPC" ] && [ -n "$DEV_VPC_ID" ] && [ "$DEV_VPC_ID" != "null" ]; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (vpc=$DEV_VPC_ID, subnet_vpc=$DEV_SUBNET_VPC)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
