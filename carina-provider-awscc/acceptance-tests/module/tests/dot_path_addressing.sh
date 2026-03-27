#!/bin/bash
# Test: module resources use dot-path addressing in state
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: module dot-path addressing"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/basic.crn" "$WORK_DIR/main.crn"
cp -r "$SCRIPT_DIR/modules" "$WORK_DIR/modules"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

# Module resources should use dot-path addressing (network.vpc, network.subnet)
# not underscore (network_vpc, network_subnet)
assert_state_value "assert: binding uses dot path (network.vpc)" \
    '[.resources[] | select(.binding == "network.vpc")] | length' \
    '1' "$WORK_DIR"

assert_state_value "assert: binding uses dot path (network.subnet)" \
    '[.resources[] | select(.binding == "network.subnet")] | length' \
    '1' "$WORK_DIR"

# Verify no underscore-style bindings exist
assert_state_value "assert: no underscore binding (network_vpc)" \
    '[.resources[] | select(.binding == "network_vpc")] | length' \
    '0' "$WORK_DIR"

assert_state_value "assert: no underscore binding (network_subnet)" \
    '[.resources[] | select(.binding == "network_subnet")] | length' \
    '0' "$WORK_DIR"

# Verify state version is v4
assert_state_value "assert: state version = 4" \
    '.version' '4' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
