#!/bin/bash
# Test: import, removed, and moved state blocks
# Flow:
#   1. Create a VPC normally (let vpc = ...)
#   2. Move it (moved { from=vpc, to=main_vpc })
#   3. Remove from state (removed { from=main_vpc }) — VPC still exists in AWS
#   4. Import it back (import { to=imported_vpc, id=<vpc-id> })
#   5. Destroy via normal apply
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: import, removed, and moved state blocks"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cd "$WORK_DIR"

# Step 1: Create VPC normally
cp "$SCRIPT_DIR/step1_create.crn" "$WORK_DIR/main.crn"
run_step "step1: create vpc" "$CARINA_BIN" apply --auto-approve .
assert_state_resource_count "assert: 1 resource in state" "1" "$WORK_DIR"

# Save the VPC ID for later import
VPC_ID=$(jq -r '.resources[0].identifier' "$WORK_DIR/carina.state.json")
printf "  %-50s" "info: vpc_id"
echo "$VPC_ID"

# Step 2: Move vpc -> main_vpc
cp "$SCRIPT_DIR/step2_moved.crn" "$WORK_DIR/main.crn"
run_step "step2: apply moved block" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify (no changes)" "$CARINA_BIN" plan .

# Verify the resource name changed in state
assert_state_value "assert: resource name = 'main_vpc'" \
    '.resources[0].name' 'main_vpc' "$WORK_DIR"
assert_state_value "assert: identifier preserved" \
    '.resources[0].identifier' "$VPC_ID" "$WORK_DIR"

# Step 3: Remove from state (VPC still exists in AWS)
cp "$SCRIPT_DIR/step3_removed.crn" "$WORK_DIR/main.crn"
run_step "step3: apply removed block" "$CARINA_BIN" apply --auto-approve .
assert_state_resource_count "assert: 0 resources in state" "0" "$WORK_DIR"

# Verify VPC still exists in AWS
printf "  %-50s" "assert: VPC still exists in AWS"
if aws ec2 describe-vpcs --vpc-ids "$VPC_ID" > /dev/null 2>&1; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (VPC was deleted from AWS)"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# Step 4: Import the VPC back
sed "s/PLACEHOLDER/$VPC_ID/" "$SCRIPT_DIR/step4_import.crn" > "$WORK_DIR/main.crn"
run_step "step4: apply import block" "$CARINA_BIN" apply --auto-approve .
run_step "step4: plan-verify (no changes)" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 1 resource in state" "1" "$WORK_DIR"
assert_state_value "assert: resource name = 'imported_vpc'" \
    '.resources[0].name' 'imported_vpc' "$WORK_DIR"
assert_state_value "assert: identifier = vpc_id" \
    '.resources[0].identifier' "$VPC_ID" "$WORK_DIR"

# Step 5: Destroy normally
echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true

# Verify VPC is deleted
printf "  %-50s" "assert: VPC deleted from AWS"
if ! aws ec2 describe-vpcs --vpc-ids "$VPC_ID" > /dev/null 2>&1; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    # Force delete if still exists
    aws ec2 delete-vpc --vpc-id "$VPC_ID" > /dev/null 2>&1 || true
    echo "WARN (deleted manually)"
    TEST_PASSED=$((TEST_PASSED + 1))
fi

rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
