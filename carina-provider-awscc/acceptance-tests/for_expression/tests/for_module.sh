#!/bin/bash
# Test: for expression with module calls (for + module iteration over map)
source "$(dirname "$0")/_helpers.sh"

echo "Test: for expression with module calls"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/for_module.crn" "$WORK_DIR/main.crn"
cp -r "$SCRIPT_DIR/modules" "$WORK_DIR/modules"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

# 2 iterations x 1 resource per module (vpc) = 2 resources
assert_state_resource_count "assert: 2 resources created" "2" "$WORK_DIR"

# Verify dot-path bindings exist
assert_state_value "assert: networks[\"dev\"].vpc exists" \
    '[.resources[] | select(.binding == "networks[\"dev\"].vpc")] | length' \
    '1' "$WORK_DIR"

assert_state_value "assert: networks[\"stg\"].vpc exists" \
    '[.resources[] | select(.binding == "networks[\"stg\"].vpc")] | length' \
    '1' "$WORK_DIR"

# Verify CIDRs are correct
assert_state_value "assert: dev vpc cidr_block = '10.0.0.0/16'" \
    '[.resources[] | select(.binding == "networks[\"dev\"].vpc")] | .[0].attributes.cidr_block' \
    '10.0.0.0/16' "$WORK_DIR"

assert_state_value "assert: stg vpc cidr_block = '10.1.0.0/16'" \
    '[.resources[] | select(.binding == "networks[\"stg\"].vpc")] | .[0].attributes.cidr_block' \
    '10.1.0.0/16' "$WORK_DIR"

# Verify tags contain environment name
assert_state_value "assert: dev vpc Name tag" \
    '[.resources[] | select(.binding == "networks[\"dev\"].vpc")] | .[0].attributes.tags.Name' \
    'for-module-test-dev' "$WORK_DIR"

assert_state_value "assert: stg vpc Name tag" \
    '[.resources[] | select(.binding == "networks[\"stg\"].vpc")] | .[0].attributes.tags.Name' \
    'for-module-test-stg' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
