#!/bin/bash
# Test: secret(env()) marks value as secret — hash in state, actual value sent to AWS
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: secret(env()) in tag value"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/secret_tag.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

# Set the secret value via environment variable
export CARINA_TEST_SECRET_VALUE="super-secret-value"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 1 resource created" "1" "$WORK_DIR"

# Verify the state does NOT contain the plaintext secret
printf "  %-50s" "assert: state does not contain plaintext"
STATE_CONTENT=$(cat "$WORK_DIR/carina.state.json")
if echo "$STATE_CONTENT" | grep -q "super-secret-value"; then
    echo "FAIL (plaintext found in state!)"
    TEST_FAILED=$((TEST_FAILED + 1))
else
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
fi

# Verify the state contains a hash for the secret tag
printf "  %-50s" "assert: state contains secret hash"
SECRET_VAL=$(jq -r '.resources[0].attributes.tags.SecretTag' "$WORK_DIR/carina.state.json" 2>/dev/null)
if echo "$SECRET_VAL" | grep -q "^_secret:argon2:"; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (expected _secret:argon2:..., got '$SECRET_VAL')"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# Verify the actual AWS resource has the real value (not the hash)
# We check by verifying plan is idempotent (no changes) — if the value
# were wrong, plan would detect a diff
run_step "step3: plan still idempotent" "$CARINA_BIN" plan .

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
