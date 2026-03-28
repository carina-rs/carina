#!/bin/bash
# Test: write-only attributes don't cause false diffs
# VPC schema has write-only attributes (ipv4_ipam_pool_id, ipv4_netmask_length).
# When not specified, plan should be idempotent (no false changes).
# When write-only values are stored in state, they should not cause issues.
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: write-only attributes - idempotent plan"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/basic.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify (no changes)" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 1 resource in state" "1" "$WORK_DIR"

# Verify no write_only_attributes in state (none were specified)
printf "  %-50s" "assert: no write_only_attributes in state"
WO_COUNT=$(jq '[.resources[] | select(.write_only_attributes | length > 0)] | length' "$WORK_DIR/carina.state.json" 2>/dev/null)
if [ "$WO_COUNT" = "0" ] || [ "$WO_COUNT" = "null" ]; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (found $WO_COUNT resources with write_only_attributes)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# Run plan again to double-check idempotency
run_step "step3: plan-verify again (still no changes)" "$CARINA_BIN" plan .

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
