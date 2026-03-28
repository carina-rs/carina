#!/bin/bash
# Test: write-only attribute full lifecycle
# 1. Create NAT Gateway with max_drain_duration_seconds=120 → plan idempotent, value in state
# 2. Change to 240 → plan detects change
# 3. Remove attribute → plan detects removal
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: write-only attribute lifecycle"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cd "$WORK_DIR"

# Step 1: Create with write-only attribute
cp "$SCRIPT_DIR/step1_create.crn" "$WORK_DIR/main.crn"
run_step "step1: apply (create with write-only attr)" "$CARINA_BIN" apply --auto-approve .
run_step "step1: plan-verify (no changes)" "$CARINA_BIN" plan .

# Verify write-only value is persisted in state
assert_state_value "assert: max_drain_duration_seconds = 120 in state" \
    '[.resources[] | select(.name | test("nat"))] | .[0].attributes.max_drain_duration_seconds' \
    '120' "$WORK_DIR"

# Verify value is in state (either API returned it or write-only persistence saved it)
printf "  %-50s" "assert: max_drain_duration_seconds in state attrs"
HAS_ATTR=$(jq '[.resources[] | select(.name | test("nat"))] | .[0].attributes | has("max_drain_duration_seconds")' "$WORK_DIR/carina.state.json" 2>/dev/null)
if [ "$HAS_ATTR" = "true" ]; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (attribute not found in state)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# Step 2: Change write-only value (120 → 240)
cp "$SCRIPT_DIR/step2_change.crn" "$WORK_DIR/main.crn"

# Plan should detect the change
printf "  %-50s" "step2: plan detects value change"
PLAN_OUTPUT=$("$CARINA_BIN" plan . 2>&1)
if echo "$PLAN_OUTPUT" | grep -q "max_drain_duration_seconds"; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (no diff detected for write-only change)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

run_step "step2: apply (update write-only attr)" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify (no changes)" "$CARINA_BIN" plan .

# Verify updated value in state
assert_state_value "assert: max_drain_duration_seconds = 240 in state" \
    '[.resources[] | select(.name | test("nat"))] | .[0].attributes.max_drain_duration_seconds' \
    '240' "$WORK_DIR"

# Step 3: Remove write-only attribute
cp "$SCRIPT_DIR/step3_remove.crn" "$WORK_DIR/main.crn"

# Plan should detect the removal
printf "  %-50s" "step3: plan detects attribute removal"
PLAN_OUTPUT=$("$CARINA_BIN" plan . 2>&1)
if echo "$PLAN_OUTPUT" | grep -q "max_drain_duration_seconds"; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (no diff detected for write-only removal)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

run_step "step3: apply (remove write-only attr)" "$CARINA_BIN" apply --auto-approve .
run_step "step3: plan-verify (no changes)" "$CARINA_BIN" plan .

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
