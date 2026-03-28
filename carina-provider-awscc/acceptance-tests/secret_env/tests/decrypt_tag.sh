#!/bin/bash
# Test: secret(decrypt()) — KMS-encrypted value decrypted and stored as hash
# Requires carina-test-000 account with alias/carina-test KMS key
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: secret(decrypt()) with KMS"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/decrypt_tag.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 1 resource created" "1" "$WORK_DIR"

# Verify state does NOT contain plaintext
printf "  %-50s" "assert: state does not contain plaintext"
if grep -q "decrypt-test-value" "$WORK_DIR/carina.state.json"; then
    echo "FAIL (plaintext found in state!)"
    TEST_FAILED=$((TEST_FAILED + 1))
else
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
fi

# Verify state contains argon2 hash for the decrypted tag
printf "  %-50s" "assert: state contains secret hash"
SECRET_VAL=$(jq -r '.resources[0].attributes.tags.DecryptedTag' "$WORK_DIR/carina.state.json" 2>/dev/null)
if echo "$SECRET_VAL" | grep -q "^_secret:argon2:"; then
    echo "OK"
    TEST_PASSED=$((TEST_PASSED + 1))
else
    echo "FAIL (expected _secret:argon2:..., got '$SECRET_VAL')"
    TEST_FAILED=$((TEST_FAILED + 1))
fi

# Verify the actual AWS resource has the decrypted value
# (plan idempotency confirms the value was sent correctly)
run_step "step3: plan still idempotent" "$CARINA_BIN" plan .

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
