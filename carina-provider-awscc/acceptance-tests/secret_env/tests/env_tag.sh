#!/bin/bash
# Test: env() function passes environment variable value to resource
source "$(dirname "$0")/../../shared/_helpers.sh"

echo "Test: env() in tag value"
echo ""

WORK_DIR=$(mktemp -d)
ACTIVE_WORK_DIR="$WORK_DIR"
cp "$SCRIPT_DIR/env_tag.crn" "$WORK_DIR/main.crn"
cd "$WORK_DIR"

# Set the environment variable
export CARINA_TEST_ENV_VALUE="production"

run_step "step1: apply" "$CARINA_BIN" apply --auto-approve .
run_step "step2: plan-verify" "$CARINA_BIN" plan .

assert_state_resource_count "assert: 1 resource created" "1" "$WORK_DIR"

# Verify the env() value was correctly applied
assert_state_value "assert: Environment tag = 'production'" \
    '.resources[0].attributes.tags.Environment' \
    'production' "$WORK_DIR"

echo "  Cleanup: destroying resources..."
"$CARINA_BIN" destroy --auto-approve . > /dev/null 2>&1 || true
rm -rf "$WORK_DIR"
ACTIVE_WORK_DIR=""

finish_test
