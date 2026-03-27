#!/bin/bash
# Test: if expression with false condition creates no resource
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: if expression (false condition)"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/if_false.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .

# With no resources, state file may not exist or have empty resources array
printf "  %-50s" "assert: 0 resources created"
if [ ! -f "$WORK_DIR/carina.state.json" ]; then
    echo "OK (no state file)"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    count=$(jq '.resources | length' "$WORK_DIR/carina.state.json" 2>/dev/null)
    if [ "$count" = "0" ]; then
        echo "OK"
        TEST_PASSED=$((TEST_PASSED + 1))
    else
        echo "FAIL (expected 0, got $count)"
        TEST_FAILED=$((TEST_FAILED + 1))
    fi
fi

rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
