#!/bin/bash
# Test: nested module imports (module calling another module)
# network_with_rt imports network module, adds route table
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: nested module imports"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/nested_module.crn" "$WORK_DIR/main.crn"
cp -r "$SCRIPT_DIR/modules" "$WORK_DIR/modules"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

# network module produces vpc + subnet, network_with_rt adds route_table = 3 real resources
assert_state_resource_count "assert: 3 resources created" "3" "$WORK_DIR"

# Verify dot-path bindings for nested resources
assert_state_value "assert: infra.net.vpc exists" \
    '[.resources[] | select(.binding == "infra.net.vpc")] | length' \
    '1' "$WORK_DIR"

assert_state_value "assert: infra.net.subnet exists" \
    '[.resources[] | select(.binding == "infra.net.subnet")] | length' \
    '1' "$WORK_DIR"

assert_state_value "assert: infra.rt exists" \
    '[.resources[] | select(.binding == "infra.rt")] | length' \
    '1' "$WORK_DIR"

# Verify route table has correct tag
assert_state_value "assert: rt Name tag" \
    '[.resources[] | select(.binding == "infra.rt")] | .[0].attributes.tags.Name' \
    'nested-module-test-rt' "$WORK_DIR"

# Verify route table references the correct VPC (from nested network module)
RT_VPC_ID=$(jq -r '[.resources[] | select(.binding == "infra.rt")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")
VPC_VPC_ID=$(jq -r '[.resources[] | select(.binding == "infra.net.vpc")] | .[0].attributes.vpc_id' "$WORK_DIR/carina.state.json")

printf "  %-50s" "assert: rt vpc_id matches nested vpc"
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
