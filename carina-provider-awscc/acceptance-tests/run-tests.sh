#!/bin/bash
# Run acceptance tests for AWSCC provider
#
# Usage (from project root):
#   ./carina-provider-awscc/acceptance-tests/run-tests.sh [command] [filter...]
#
# Commands:
#   validate   - Validate .crn files (default, no AWS credentials needed)
#   plan       - Run plan on .crn files (single account, needs aws-vault)
#   apply      - Apply .crn files (single account, needs aws-vault)
#   destroy    - Destroy resources created by .crn files (single account)
#   full       - Run apply+plan-verify+destroy per test, 10 accounts in parallel
#   cleanup    - Run destroy across all 10 accounts in parallel (recover from stuck state)
#
# For validate, no AWS credentials are needed.
# For plan/apply/destroy, wrap with: aws-vault exec <profile> -- ./run-tests.sh ...
# For full, aws-vault is called internally per account (carina-test-000..009).
#
# Filter (optional):
#   One or more substrings to match against test paths.
#   A test is included if it matches ANY of the provided filters (OR logic).
#   When no filter is provided, all tests are included.
#
# Examples:
#   ./run-tests.sh validate                          # validate all
#   ./run-tests.sh validate ec2_vpc/basic            # validate specific test
#   ./run-tests.sh full                              # apply+plan-verify+destroy, 10 parallel accounts
#   ./run-tests.sh full ec2_vpc                      # apply+plan-verify+destroy VPC tests only
#   ./run-tests.sh full ec2_ipam ec2_vpc/with_ipam   # multiple filters in single invocation
#   ./run-tests.sh cleanup                           # destroy all matching tests across 10 accounts
#   ./run-tests.sh cleanup ec2_vpc                   # destroy VPC tests only across 10 accounts

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

COMMAND="${1:-validate}"
shift || true
FILTERS=("$@")

# ── File-based lock to prevent concurrent executions ──────────────────
# Uses mkdir (atomic on POSIX) + PID file for stale lock detection.
# Applies to all commands except 'validate' which doesn't use AWS.
LOCK_DIR="/tmp/carina-acceptance-tests.lock"

acquire_lock() {
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        echo $$ > "$LOCK_DIR/pid"
        return 0
    fi

    # Lock exists - check if the holding process is still alive
    if [ -f "$LOCK_DIR/pid" ]; then
        OLD_PID=$(cat "$LOCK_DIR/pid" 2>/dev/null || echo "")
        if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
            echo "ERROR: Another run-tests.sh instance is already running (PID $OLD_PID, lock: $LOCK_DIR)"
            return 1
        fi
    fi

    # Stale lock (process no longer running) - reclaim it
    echo "Removing stale lock (previous process no longer running)"
    rm -rf "$LOCK_DIR"
    mkdir "$LOCK_DIR" 2>/dev/null || {
        echo "ERROR: Failed to acquire lock (race condition). Retry."
        return 1
    }
    echo $$ > "$LOCK_DIR/pid"
    return 0
}

release_lock() {
    rm -rf "$LOCK_DIR"
}

if [ "$COMMAND" != "validate" ]; then
    if ! acquire_lock; then
        exit 1
    fi
    trap release_lock EXIT
fi

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
    validate|plan|apply|destroy|full|cleanup)
        ;;
    *)
        echo "ERROR: Unknown command '$COMMAND'"
        echo "Usage: $0 [validate|plan|apply|destroy|full|cleanup] [filter...]"
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
# matches_any_filter: returns 0 if rel_path matches any filter, or if no filters given
matches_any_filter() {
    local rel_path="$1"
    if [ ${#FILTERS[@]} -eq 0 ]; then
        return 0
    fi
    for f in "${FILTERS[@]}"; do
        if [[ "$rel_path" == *"$f"* ]]; then
            return 0
        fi
    done
    return 1
}

TESTS=()
while IFS= read -r -d '' file; do
    REL_PATH="${file#$SCRIPT_DIR/}"
    if ! matches_any_filter "$REL_PATH"; then
        continue
    fi
    TESTS+=("$file")
done < <(find "$SCRIPT_DIR" -name "*.crn" -print0 | sort -z)

if [ ${#TESTS[@]} -eq 0 ]; then
    if [ ${#FILTERS[@]} -gt 0 ]; then
        echo "No test files found matching: ${FILTERS[*]}"
    else
        echo "No test files found"
    fi
    exit 0
fi

# ── cleanup: destroy across all 10 accounts in parallel ───────────────
if [ "$COMMAND" = "cleanup" ]; then
    TOTAL=${#TESTS[@]}
    echo "Running cleanup (destroy) on $TOTAL test(s) across $NUM_ACCOUNTS accounts"
    echo ""

    WORK_DIR=$(mktemp -d)
    trap "rm -rf $WORK_DIR; release_lock" EXIT

    # Pre-authenticate all accounts
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

    # Each account tries to destroy all tests (we don't know which account
    # created which resources, so every account attempts every test)
    PIDS=()
    for SLOT in $(seq 0 $((NUM_ACCOUNTS - 1))); do
        ACCOUNT="${ACCOUNTS[$SLOT]}"
        LOG_FILE="$WORK_DIR/slot_${SLOT}.log"
        STATE_DIR="$WORK_DIR/state_${SLOT}"
        mkdir -p "$STATE_DIR"

        (
            set +e
            DESTROYED=0
            SKIPPED=0

            for TEST_FILE in "${TESTS[@]}"; do
                REL_PATH="${TEST_FILE#$SCRIPT_DIR/}"
                echo "RUNNING destroy $REL_PATH"
                DESTROY_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1)
                DESTROY_RC=$?
                if [ $DESTROY_RC -eq 0 ]; then
                    if echo "$DESTROY_OUTPUT" | grep -q "No resources to destroy"; then
                        SKIPPED=$((SKIPPED + 1))
                    else
                        echo "DESTROYED $REL_PATH"
                        DESTROYED=$((DESTROYED + 1))
                    fi
                else
                    echo "FAIL (destroy) $REL_PATH"
                    echo "  ERROR: $DESTROY_OUTPUT"
                fi
            done

            echo "---"
            echo "SUMMARY $ACCOUNT: $DESTROYED destroyed, $SKIPPED already clean"
        ) > "$LOG_FILE" 2>&1 &

        PIDS+=($!)
        echo "  [$ACCOUNT] started (PID $!)"
    done

    echo ""
    echo "Logs: $WORK_DIR/slot_*.log"
    echo "Waiting for all accounts to finish cleanup..."
    echo ""

    # Wait for all workers
    OVERALL_EXIT=0
    for PID in "${PIDS[@]}"; do
        wait "$PID" || OVERALL_EXIT=1
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

    echo "════════════════════════════════════════"
    echo "Cleanup complete."
    echo "════════════════════════════════════════"

    exit $OVERALL_EXIT
fi

# ── full: 10-account parallel execution ──────────────────────────────
if [ "$COMMAND" = "full" ]; then
    TOTAL=${#TESTS[@]}
    echo "Running full cycle (apply -> plan-verify -> destroy) on $TOTAL test(s) across $NUM_ACCOUNTS accounts"
    echo ""

    WORK_DIR=$(mktemp -d)

    # Signal handling for the main process: forward signals to workers
    PIDS=()
    cleanup_main() {
        echo ""
        echo "Interrupted! Signaling workers to clean up..."
        for PID in "${PIDS[@]}"; do
            kill -TERM "$PID" 2>/dev/null || true
        done
        echo "Waiting for workers to finish cleanup..."
        for PID in "${PIDS[@]}"; do
            wait "$PID" 2>/dev/null || true
        done
        echo "All workers finished."
        rm -rf "$WORK_DIR"
        release_lock
        exit 1
    }
    trap cleanup_main INT TERM

    # Clean up temp dir and release lock on normal exit
    trap "rm -rf $WORK_DIR; release_lock" EXIT

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
            CURRENT_TEST_FILE=""
            INTERRUPTED=0

            # Worker trap: attempt destroy for the current test on interruption
            worker_cleanup() {
                INTERRUPTED=1
                if [ -n "$CURRENT_TEST_FILE" ]; then
                    REL_PATH="${CURRENT_TEST_FILE#$SCRIPT_DIR/}"
                    echo "INTERRUPTED - destroying resources for $REL_PATH"
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$CURRENT_TEST_FILE" 2>&1 || true
                fi
            }
            trap worker_cleanup INT TERM

            while IFS= read -r TEST_FILE; do
                if [ $INTERRUPTED -eq 1 ]; then
                    break
                fi

                REL_PATH="${TEST_FILE#$SCRIPT_DIR/}"
                CURRENT_TEST_FILE="$TEST_FILE"

                # Apply (run from STATE_DIR so each account has its own state file)
                echo "RUNNING apply $REL_PATH"
                APPLY_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" apply --auto-approve "$TEST_FILE" 2>&1)
                APPLY_RC=$?
                if [ $INTERRUPTED -eq 1 ]; then
                    break
                fi
                if [ $APPLY_RC -ne 0 ] || echo "$APPLY_OUTPUT" | grep -q "failed"; then
                    echo "FAIL (apply) $REL_PATH"
                    echo "  ERROR: $APPLY_OUTPUT"
                    FAILED=$((FAILED + 1))
                    # Try to destroy whatever was partially created
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1 || true
                    CURRENT_TEST_FILE=""
                    continue
                fi

                # Post-apply plan verification (idempotency check)
                echo "RUNNING plan-verify $REL_PATH"
                PLAN_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" plan --detailed-exitcode "$TEST_FILE" 2>&1)
                PLAN_RC=$?
                if [ $INTERRUPTED -eq 1 ]; then
                    break
                fi
                if [ $PLAN_RC -eq 2 ]; then
                    echo "FAIL (plan-verify) $REL_PATH"
                    echo "  ERROR: Post-apply plan detected changes (not idempotent):"
                    echo "  $PLAN_OUTPUT"
                    FAILED=$((FAILED + 1))
                    # Still destroy to clean up
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1 || true
                    CURRENT_TEST_FILE=""
                    continue
                elif [ $PLAN_RC -ne 0 ]; then
                    echo "FAIL (plan-verify) $REL_PATH"
                    echo "  ERROR: $PLAN_OUTPUT"
                    FAILED=$((FAILED + 1))
                    cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1 || true
                    CURRENT_TEST_FILE=""
                    continue
                fi

                # Destroy
                echo "RUNNING destroy $REL_PATH"
                DESTROY_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$TEST_FILE" 2>&1)
                DESTROY_RC=$?
                if [ $DESTROY_RC -ne 0 ] || echo "$DESTROY_OUTPUT" | grep -q "failed"; then
                    echo "FAIL (destroy) $REL_PATH"
                    echo "  ERROR: $DESTROY_OUTPUT"
                    FAILED=$((FAILED + 1))
                    CURRENT_TEST_FILE=""
                    continue
                fi

                echo "OK   $REL_PATH"
                PASSED=$((PASSED + 1))
                CURRENT_TEST_FILE=""
            done < "$LIST_FILE"

            echo "---"
            echo "SUMMARY $ACCOUNT: $PASSED passed, $FAILED failed"
        ) > "$LOG_FILE" 2>&1 &

        PIDS+=($!)
        echo "  [$ACCOUNT] started (PID $!) - $(wc -l < "$LIST_FILE" | tr -d ' ') test(s)"
    done

    echo ""
    echo "Logs: $WORK_DIR/slot_*.log"
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
