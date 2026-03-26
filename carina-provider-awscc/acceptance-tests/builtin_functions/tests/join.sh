#!/bin/bash
# Test: join() function
source "$(dirname "$0")/_helpers.sh"

echo "Test: join() function"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/join.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .
assert_state_value "assert: tag Name = 'web-test-vpc'" '.resources[0].attributes.tags.Name' 'web-test-vpc' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
