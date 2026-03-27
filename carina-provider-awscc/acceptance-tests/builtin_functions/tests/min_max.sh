#!/bin/bash
# Test: min() and max() functions
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: min() and max() functions"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/min_max.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .
assert_state_value "assert: tag MinPort = 'port-8080'" '.resources[0].attributes.tags.MinPort' 'port-8080' "$WORK_DIR"
assert_state_value "assert: tag MaxPort = 'port-9090'" '.resources[0].attributes.tags.MaxPort' 'port-9090' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
