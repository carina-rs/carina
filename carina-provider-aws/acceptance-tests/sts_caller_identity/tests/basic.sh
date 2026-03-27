#!/bin/bash
# Test: sts_caller_identity data source (read)
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: sts_caller_identity data source"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/basic.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply (read data source)" "$CARINA_BIN" apply --auto-approve .

# Verify state contains the caller identity resource
assert_state_resource_count "assert: 1 resource in state" "1" "$WORK_DIR"

# account_id should be a 12-digit number
printf "  %-50s" "assert: account_id is 12 digits"
ACCOUNT_ID=$(jq -r '.resources[0].attributes.account_id' "$WORK_DIR/carina.state.json" 2>/dev/null)
if echo "$ACCOUNT_ID" | grep -qE '^[0-9]{12}$'; then
    echo "OK ($ACCOUNT_ID)"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (got '$ACCOUNT_ID')"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# arn should start with arn:aws
printf "  %-50s" "assert: arn starts with arn:aws"
ARN=$(jq -r '.resources[0].attributes.arn' "$WORK_DIR/carina.state.json" 2>/dev/null)
if echo "$ARN" | grep -q '^arn:aws'; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (got '$ARN')"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# No cleanup needed — read-only data source creates no infrastructure
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
