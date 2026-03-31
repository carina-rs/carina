#!/usr/bin/env bash
# Cascading update without create_before_destroy test
#
# Tests that when a VPC is replaced (without CBD),
# dependent resources (subnet) appear as cascading updates in the plan.
#
# Usage:
#   aws-vault exec carina-test-000 -- ./carina-provider-awscc/acceptance-tests/cascade_without_cbd/run.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CARINA="cargo run --bin carina --"

# ── Provider source injection ────────────────────────────────────────
AWSCC_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-awscc"
AWS_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-aws"

inject_provider_source() {
    local original="$1"
    local tmp_file
    tmp_file=$(mktemp "${TMPDIR:-/tmp}/carina-test-XXXXXX.crn")
    sed \
        -e '/^provider awscc {/a\
  source = "file://'"$AWSCC_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        -e '/^provider aws {/a\
  source = "file://'"$AWS_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        "$original" > "$tmp_file"
    echo "$tmp_file"
}

STEP1=$(inject_provider_source "$SCRIPT_DIR/step1.crn")
STEP2=$(inject_provider_source "$SCRIPT_DIR/step2.crn")
trap "rm -f $STEP1 $STEP2" EXIT

PASS=0
FAIL=0

run_step() {
    local description="$1"
    local command="$2"
    shift 2

    echo "── $description ──"
    if eval "$command" "$@"; then
        echo "  ✓ $description"
        PASS=$((PASS + 1))
    else
        echo "  ✗ $description"
        FAIL=$((FAIL + 1))
    fi
}

echo ""
echo "════════════════════════════════════════"
echo " Cascade without create_before_destroy"
echo "════════════════════════════════════════"
echo ""

# Step 1: Apply initial state (VPC + SG + Ingress)
run_step "apply step1 (create VPC + subnet)" "$CARINA apply --auto-approve $STEP1"

# Step 2: Plan with changed group_description
# The plan should show:
#   -/+ SG (replace, forces replacement)
#   ~ ingress rule (cascading update)
echo ""
echo "── plan step2 (expect cascade) ──"
PLAN_OUTPUT=$($CARINA plan "$STEP2" 2>&1) || true
echo "$PLAN_OUTPUT"

if echo "$PLAN_OUTPUT" | grep -q "create before destroy"; then
    echo "  ✓ create_before_destroy auto-detected in plan"
    PASS=$((PASS + 1))
else
    echo "  ✗ create_before_destroy NOT auto-detected in plan"
    FAIL=$((FAIL + 1))
fi

# Step 2: Apply (VPC CBD replace + subnet replace)
run_step "apply step2 (replace VPC + subnet)" "$CARINA apply --auto-approve $STEP2"

# Step 2: Plan verify (should show no changes after apply)
echo ""
echo "── plan-verify step2 ──"
VERIFY_OUTPUT=$($CARINA plan "$STEP2" 2>&1) || true
echo "$VERIFY_OUTPUT"

if echo "$VERIFY_OUTPUT" | grep -q "No changes"; then
    echo "  ✓ plan-verify: no changes (idempotent)"
    PASS=$((PASS + 1))
else
    echo "  ✗ plan-verify: unexpected changes detected"
    FAIL=$((FAIL + 1))
fi

# Cleanup: destroy
run_step "destroy (cleanup)" "$CARINA destroy --auto-approve $STEP2"

echo ""
echo "════════════════════════════════════════"
echo "Total: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════"

[ "$FAIL" -eq 0 ]
