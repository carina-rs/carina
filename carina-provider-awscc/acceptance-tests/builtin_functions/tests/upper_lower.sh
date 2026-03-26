#!/bin/bash
# Test: upper() and lower() functions
source "$(dirname "$0")/_helpers.sh"

echo "Test: upper() and lower() functions"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/upper_lower.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .
assert_state_value "assert: tag Name = 'PRODUCTION'" '.resources[0].attributes.tags.Name' 'PRODUCTION' "$WORK_DIR"
assert_state_value "assert: tag App = 'webapp'" '.resources[0].attributes.tags.App' 'webapp' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
