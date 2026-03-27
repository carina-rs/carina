#!/bin/bash
# Test: for expression over a map
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: for expression over map"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/for_map.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 2 resources created" "2" "$WORK_DIR"

# Map iteration produces resources keyed by map key
assert_state_value "assert: vpcs[\"dev\"] Name = 'for-map-test-dev'" \
    '[.resources[] | select(.binding == "vpcs[\"dev\"]")] | .[0].attributes.tags.Name' \
    'for-map-test-dev' "$WORK_DIR"

assert_state_value "assert: vpcs[\"dev\"] cidr_block = '10.0.0.0/16'" \
    '[.resources[] | select(.binding == "vpcs[\"dev\"]")] | .[0].attributes.cidr_block' \
    '10.0.0.0/16' "$WORK_DIR"

assert_state_value "assert: vpcs[\"stg\"] Name = 'for-map-test-stg'" \
    '[.resources[] | select(.binding == "vpcs[\"stg\"]")] | .[0].attributes.tags.Name' \
    'for-map-test-stg' "$WORK_DIR"

assert_state_value "assert: vpcs[\"stg\"] cidr_block = '10.1.0.0/16'" \
    '[.resources[] | select(.binding == "vpcs[\"stg\"]")] | .[0].attributes.cidr_block' \
    '10.1.0.0/16' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
