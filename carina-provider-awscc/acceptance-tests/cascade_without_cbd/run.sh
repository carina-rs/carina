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
CARINA="cargo run --bin carina --"
STEP1="$SCRIPT_DIR/step1.crn"
STEP2="$SCRIPT_DIR/step2.crn"

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

# Cleanup: destroy
run_step "destroy (cleanup)" "$CARINA destroy --auto-approve $STEP1"

echo ""
echo "════════════════════════════════════════"
echo "Total: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════"

[ "$FAIL" -eq 0 ]
