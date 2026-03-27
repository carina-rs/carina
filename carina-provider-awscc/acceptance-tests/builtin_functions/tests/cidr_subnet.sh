#!/bin/bash
# Test: cidr_subnet() function
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: cidr_subnet() function"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/cidr_subnet.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .
assert_state_value \
    "assert: subnet cidr_block = '10.0.1.0/24'" \
    '.resources[] | select(.resource_type == "ec2.subnet") | .attributes.cidr_block' \
    '10.0.1.0/24' \
    "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
