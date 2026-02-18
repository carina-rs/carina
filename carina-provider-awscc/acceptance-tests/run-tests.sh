#!/bin/bash
# Run acceptance tests for AWSCC provider
#
# Usage (from project root):
#   ./carina-provider-awscc/acceptance-tests/run-tests.sh [command] [filter]
#
# Commands:
#   validate   - Validate .crn files (default, no AWS credentials needed)
#   plan       - Run plan on .crn files (single account, needs aws-vault)
#   apply      - Apply .crn files (single account, needs aws-vault)
#   destroy    - Destroy resources created by .crn files (single account)
#   full       - Run apply+plan-verify+destroy per test, 10 accounts in parallel
#
# For validate, no AWS credentials are needed.
# For plan/apply/destroy, wrap with: aws-vault exec <profile> -- ./run-tests.sh ...
# For full, aws-vault is called internally per account (carina-test-000..009).
#
# Filter (optional):
#   A substring to match against test paths.
#
# Examples:
#   ./run-tests.sh validate                          # validate all
#   ./run-tests.sh validate ec2_vpc/basic            # validate specific test
#   ./run-tests.sh full                              # apply+plan-verify+destroy, 10 parallel accounts
#   ./run-tests.sh full ec2_vpc                      # apply+plan-verify+destroy VPC tests only

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

COMMAND="${1:-validate}"
FILTER="${2:-}"

ACCOUNTS=(
    carina-test-000
    carina-test-001
    carina-test-002
    carina-test-003
    carina-test-004
    carina-test-005
    carina-test-006
    carina-test-007
    carina-test-008
    carina-test-009
)
NUM_ACCOUNTS=${#ACCOUNTS[@]}

# Validate command
case "$COMMAND" in
    validate|plan|apply|destroy|full)
        ;;
    *)
        echo "ERROR: Unknown command '$COMMAND'"
        echo "Usage: $0 [validate|plan|apply|destroy|full] [filter]"
        exit 1
        ;;
esac

# Build carina first
echo "Building carina..."
cargo build --quiet 2>/dev/null || cargo build
echo ""

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "ERROR: carina binary not found at $CARINA_BIN"
    exit 1
fi

# Find test files
TESTS=()
while IFS= read -r -d '' file; do
    REL_PATH="${file#$SCRIPT_DIR/}"
    if [ -n "$FILTER" ] && [[ "$REL_PATH" != *"$FILTER"* ]]; then
        continue
    fi
    TESTS+=("$file")
done < <(find "$SCRIPT_DIR" -name "*.crn" -print0 | sort -z)

if [ ${#TESTS[@]} -eq 0 ]; then
    echo "No test files found${FILTER:+ matching '$FILTER'}"
    exit 0
fi

# ── full: 10-account parallel execution ──────────────────────────────
if [ "$COMMAND" = "full" ]; then
    TOTAL=${#TESTS[@]}
    echo "Running full cycle (apply -> plan-verify -> destroy) on $TOTAL test(s) across $NUM_ACCOUNTS accounts"
    echo ""

    WORK_DIR=$(mktemp -d)
    trap "rm -rf $WORK_DIR" EXIT

    # Pre-authenticate all accounts sequentially to avoid opening
    # multiple SSO browser tabs simultaneously
    echo "Pre-authenticating AWS accounts..."
    for SLOT in $(seq 0 $((NUM_ACCOUNTS - 1))); do
        ACCOUNT="${ACCOUNTS[$SLOT]}"
        echo "  Authenticating $ACCOUNT..."
        if ! aws-vault exec "$ACCOUNT" -- true 2>&1; then
            echo "  WARNING: Failed to pre-authenticate $ACCOUNT"
        fi
    done
    echo "Pre-authentication complete."
    echo ""

    # Distribute tests round-robin across accounts
    for i in "${!TESTS[@]}"; do
        SLOT=$((i % NUM_ACCOUNTS))
        echo "${TESTS[$i]}" >> "$WORK_DIR/slot_${SLOT}.list"
    done

    # Launch one worker per account
    PIDS=()
    for SLOT in $(seq 0 $((NUM_ACCOUNTS - 1))); do
        LIST_FILE="$WORK_DIR/slot_${SLOT}.list"
        if [ ! -f "$LIST_FILE" ]; then
            continue
        fi

        ACCOUNT="${ACCOUNTS[$SLOT]}"
        LOG_FILE="$WORK_DIR/slot_${SLOT}.log"
        STATE_DIR="$WORK_DIR/state_${SLOT}"
        mkdir -p "$STATE_DIR"

        (
            set +e
            PASSED=0
            FAILED=0

            while IFS= read -r TEST_FILE; do
                REL_PATH="${TEST_FILE#$SCRIPT_DIR/}"

                # Apply (run from STATE_DIR so each account has its own state file)
                APPLY_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" apply --auto-approve "$TEST_FILE" 2>&1)
                APPLY_RC=$?
                if [ $APPLY_RC -ne 0 ] || echo "$APPLY_OUTPUT" | grep -q "failed"; then
                    echo "FAIL (apply) $REL_PATH"
                    echo "  ERROR: $APPLY_OUTPUT"
                    FAILED=$((FAILED + 1))
                    # Try to destroy whatever was partially created
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1 || true
                    continue
                fi

                # Post-apply plan verification (idempotency check)
                PLAN_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" plan --detailed-exitcode "$TEST_FILE" 2>&1)
                PLAN_RC=$?
                if [ $PLAN_RC -eq 2 ]; then
                    echo "FAIL (plan-verify) $REL_PATH"
                    echo "  ERROR: Post-apply plan detected changes (not idempotent):"
                    echo "  $PLAN_OUTPUT"
                    FAILED=$((FAILED + 1))
                    # Still destroy to clean up
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1 || true
                    continue
                elif [ $PLAN_RC -ne 0 ]; then
                    echo "FAIL (plan-verify) $REL_PATH"
                    echo "  ERROR: $PLAN_OUTPUT"
                    FAILED=$((FAILED + 1))
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1 || true
                    continue
                fi

                # Destroy
                DESTROY_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1)
                DESTROY_RC=$?
                if [ $DESTROY_RC -ne 0 ] || echo "$DESTROY_OUTPUT" | grep -q "failed"; then
                    echo "FAIL (destroy) $REL_PATH"
                    echo "  ERROR: $DESTROY_OUTPUT"
                    FAILED=$((FAILED + 1))
                    continue
                fi

                echo "OK   $REL_PATH"
                PASSED=$((PASSED + 1))
            done < "$LIST_FILE"

            echo "---"
            echo "SUMMARY $ACCOUNT: $PASSED passed, $FAILED failed"
        ) > "$LOG_FILE" 2>&1 &

        PIDS+=($!)
        echo "  [$ACCOUNT] started (PID $!) - $(wc -l < "$LIST_FILE" | tr -d ' ') test(s)"
    done

    echo ""
    echo "Waiting for all accounts to finish..."
    echo ""

    # Wait and collect results
    OVERALL_EXIT=0
    for SLOT in $(seq 0 $((NUM_ACCOUNTS - 1))); do
        LOG_FILE="$WORK_DIR/slot_${SLOT}.log"
        if [ ! -f "$LOG_FILE" ] && [ $SLOT -ge ${#PIDS[@]} ]; then
            continue
        fi
        PID_IDX=$SLOT
        if [ $PID_IDX -lt ${#PIDS[@]} ]; then
            wait "${PIDS[$PID_IDX]}" || OVERALL_EXIT=1
        fi
    done

    # Print results per account
    for SLOT in $(seq 0 $((NUM_ACCOUNTS - 1))); do
        LOG_FILE="$WORK_DIR/slot_${SLOT}.log"
        if [ ! -f "$LOG_FILE" ]; then
            continue
        fi
        ACCOUNT="${ACCOUNTS[$SLOT]}"
        echo "── $ACCOUNT ──"
        cat "$LOG_FILE"
        echo ""
    done

    # Aggregate
    TOTAL_PASSED=0
    TOTAL_FAILED=0
    for SLOT in $(seq 0 $((NUM_ACCOUNTS - 1))); do
        LOG_FILE="$WORK_DIR/slot_${SLOT}.log"
        if [ ! -f "$LOG_FILE" ]; then
            continue
        fi
        P=$(grep "^SUMMARY" "$LOG_FILE" | sed 's/.*: \([0-9]*\) passed.*/\1/' || echo 0)
        F=$(grep "^SUMMARY" "$LOG_FILE" | sed 's/.*, \([0-9]*\) failed/\1/' || echo 0)
        TOTAL_PASSED=$((TOTAL_PASSED + P))
        TOTAL_FAILED=$((TOTAL_FAILED + F))
    done

    echo "════════════════════════════════════════"
    echo "Total: $TOTAL_PASSED passed, $TOTAL_FAILED failed (of $TOTAL)"
    echo "════════════════════════════════════════"

    exit $OVERALL_EXIT
fi

# ── Single-command mode (validate/plan/apply/destroy) ────────────────
echo "Running '$COMMAND' on ${#TESTS[@]} test file(s):"
echo ""

PASSED=0
FAILED=0
ERRORS=()

for TEST_FILE in "${TESTS[@]}"; do
    REL_PATH="${TEST_FILE#$SCRIPT_DIR/}"
    printf "  %-55s " "$REL_PATH"

    AUTO_APPROVE=""
    if [ "$COMMAND" = "apply" ] || [ "$COMMAND" = "destroy" ]; then
        AUTO_APPROVE="--auto-approve"
    fi
    if OUTPUT=$("$CARINA_BIN" "$COMMAND" $AUTO_APPROVE "$TEST_FILE" 2>&1); then
        if [ "$COMMAND" = "apply" ]; then
            # Post-apply plan verification (idempotency check)
            PLAN_OUTPUT=$("$CARINA_BIN" plan --detailed-exitcode "$TEST_FILE" 2>&1)
            PLAN_RC=$?
            if [ $PLAN_RC -eq 2 ]; then
                echo "FAIL (plan-verify)"
                ERRORS+=("$REL_PATH: Post-apply plan detected changes (not idempotent): $PLAN_OUTPUT")
                FAILED=$((FAILED + 1))
                continue
            elif [ $PLAN_RC -ne 0 ]; then
                echo "FAIL (plan-verify)"
                ERRORS+=("$REL_PATH: $PLAN_OUTPUT")
                FAILED=$((FAILED + 1))
                continue
            fi
        fi
        echo "OK"
        PASSED=$((PASSED + 1))
    else
        echo "FAIL"
        ERRORS+=("$REL_PATH: $OUTPUT")
        FAILED=$((FAILED + 1))
    fi
done

echo ""
echo "Results: $PASSED passed, $FAILED failed (total: $((PASSED + FAILED)))"

if [ ${#ERRORS[@]} -gt 0 ]; then
    echo ""
    echo "Failures:"
    for ERR in "${ERRORS[@]}"; do
        echo "  $ERR"
    done
    exit 1
fi
