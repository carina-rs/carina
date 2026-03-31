#!/bin/bash
# Run acceptance tests for AWS provider
#
# Usage (from project root):
#   ./carina-provider-aws/acceptance-tests/run-tests.sh [options] [command] [filter...]
#
# Options:
#   --accounts START-END  Use only accounts in range (e.g., 0-3, 4-6, 7-9)
#                         Enables concurrent runs with non-overlapping account pools
#
# Commands:
#   validate   - Validate .crn files (default, no AWS credentials needed)
#   plan       - Run plan on .crn files (single account, needs aws-vault)
#   apply      - Apply .crn files (single account, needs aws-vault)
#   destroy    - Destroy resources created by .crn files (single account)
#   full       - Run apply+plan-verify+destroy per test, accounts in parallel
#   cleanup    - Run destroy across accounts in parallel (recover from stuck state)
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
#   ./run-tests.sh validate s3_bucket/basic          # validate specific test
#   ./run-tests.sh full                              # apply+plan-verify+destroy, 10 parallel accounts
#   ./run-tests.sh full ec2_vpc                      # apply+plan-verify+destroy VPC tests only
#   ./run-tests.sh full --accounts 0-3 iam_role      # use only accounts 000-003
#   ./run-tests.sh full --accounts 4-6 ec2_vpc       # concurrent run with accounts 004-006
#   ./run-tests.sh cleanup                           # destroy all matching tests across 10 accounts

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ── Parse options ─────────────────────────────────────────────────────
ACCOUNT_START=""
ACCOUNT_END=""

# Parse --accounts before command
ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --accounts)
            if [ -z "${2:-}" ]; then
                echo "ERROR: --accounts requires a range argument (e.g., 0-3)"
                exit 1
            fi
            RANGE="$2"
            ACCOUNT_START="${RANGE%-*}"
            ACCOUNT_END="${RANGE#*-}"
            if ! echo "$ACCOUNT_START" | grep -q '^[0-9]\{1,\}$' || ! echo "$ACCOUNT_END" | grep -q '^[0-9]\{1,\}$'; then
                echo "ERROR: --accounts range must be START-END (e.g., 0-3)"
                exit 1
            fi
            if [ "$ACCOUNT_START" -gt "$ACCOUNT_END" ]; then
                echo "ERROR: --accounts START must be <= END (got $ACCOUNT_START-$ACCOUNT_END)"
                exit 1
            fi
            if [ "$ACCOUNT_START" -gt 9 ] || [ "$ACCOUNT_END" -gt 9 ]; then
                echo "ERROR: --accounts range must be within 0-9"
                exit 1
            fi
            shift 2
            ;;
        *)
            ARGS+=("$1")
            shift
            ;;
    esac
done

set -- ${ARGS[@]+"${ARGS[@]}"}

COMMAND="${1:-validate}"
shift || true
FILTERS=("$@")

# ── Build account list ────────────────────────────────────────────────
ALL_ACCOUNTS=(
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

if [ -n "$ACCOUNT_START" ]; then
    ACCOUNTS=()
    ACCOUNT_INDICES=()
    for i in $(seq "$ACCOUNT_START" "$ACCOUNT_END"); do
        ACCOUNTS+=("${ALL_ACCOUNTS[$i]}")
        ACCOUNT_INDICES+=("$i")
    done
else
    ACCOUNTS=("${ALL_ACCOUNTS[@]}")
    ACCOUNT_INDICES=()
    for i in $(seq 0 9); do
        ACCOUNT_INDICES+=("$i")
    done
fi
NUM_ACCOUNTS=${#ACCOUNTS[@]}

# ── Per-account lock files ────────────────────────────────────────────
# Each account gets its own lock to allow concurrent runs with
# non-overlapping account pools.
LOCK_BASE="/tmp/carina-acceptance-tests"
LOCKED_ACCOUNTS=()

acquire_account_lock() {
    local account_index="$1"
    local lock_dir="${LOCK_BASE}-${account_index}.lock"

    if mkdir "$lock_dir" 2>/dev/null; then
        echo $$ > "$lock_dir/pid"
        LOCKED_ACCOUNTS+=("$account_index")
        return 0
    fi

    # Lock exists - check if the holding process is still alive
    if [ -f "$lock_dir/pid" ]; then
        OLD_PID=$(cat "$lock_dir/pid" 2>/dev/null || echo "")
        if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
            echo "ERROR: Account ${account_index} is already locked by another run-tests.sh instance (PID $OLD_PID)"
            return 1
        fi
    fi

    # Stale lock - reclaim
    echo "Removing stale lock for account ${account_index} (previous process no longer running)"
    rm -rf "$lock_dir"
    mkdir "$lock_dir" 2>/dev/null || {
        echo "ERROR: Failed to acquire lock for account ${account_index} (race condition). Retry."
        return 1
    }
    echo $$ > "$lock_dir/pid"
    LOCKED_ACCOUNTS+=("$account_index")
    return 0
}

release_account_locks() {
    for account_index in ${LOCKED_ACCOUNTS[@]+"${LOCKED_ACCOUNTS[@]}"}; do
        rm -rf "${LOCK_BASE}-${account_index}.lock"
    done
    LOCKED_ACCOUNTS=()
}

acquire_all_account_locks() {
    for account_index in "${ACCOUNT_INDICES[@]}"; do
        if ! acquire_account_lock "$account_index"; then
            # Release any locks we already acquired
            release_account_locks
            return 1
        fi
    done
    return 0
}

if [ "$COMMAND" != "validate" ]; then
    if ! acquire_all_account_locks; then
        exit 1
    fi
    trap release_account_locks EXIT
fi

# Validate command
case "$COMMAND" in
    validate|plan|apply|destroy|full|cleanup)
        ;;
    *)
        echo "ERROR: Unknown command '$COMMAND'"
        echo "Usage: $0 [--accounts START-END] [validate|plan|apply|destroy|full|cleanup] [filter...]"
        exit 1
        ;;
esac

# Build carina and provider binaries
echo "Building carina..."
cargo build --quiet 2>/dev/null || cargo build
echo "Building provider binaries..."
cargo build -p carina-provider-aws --bin carina-provider-aws --quiet 2>/dev/null || cargo build -p carina-provider-aws --bin carina-provider-aws
cargo build -p carina-provider-awscc --bin carina-provider-awscc --quiet 2>/dev/null || cargo build -p carina-provider-awscc --bin carina-provider-awscc
echo ""

CARINA_BIN="$PROJECT_ROOT/target/debug/carina"
if [ ! -f "$CARINA_BIN" ]; then
    echo "ERROR: carina binary not found at $CARINA_BIN"
    exit 1
fi

# ── Provider source injection ────────────────────────────────────────
# After Phase 4, provider blocks require source and version attributes.
# Inject them dynamically so .crn files don't need to hard-code binary paths.
AWS_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-aws"
AWSCC_PROVIDER_BIN="$PROJECT_ROOT/target/debug/carina-provider-awscc"

# inject_provider_source: Create a temp copy of a .crn file with source/version
# injected into provider blocks. Prints the temp file path.
# Args: original_crn_file
inject_provider_source() {
    local original="$1"
    local tmp_file
    tmp_file=$(mktemp).crn

    sed \
        -e '/^provider aws {/a\
  source = "file://'"$AWS_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        -e '/^provider awscc {/a\
  source = "file://'"$AWSCC_PROVIDER_BIN"'"\
  version = "0.1.0"' \
        "$original" > "$tmp_file"

    echo "$tmp_file"
}

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
    # Skip .crn files in directories with a custom run.sh (multi-step tests)
    DIR_OF_FILE="$(dirname "$file")"
    if [ -f "$DIR_OF_FILE/run.sh" ]; then
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

# ── cleanup: destroy across accounts in parallel ─────────────────────
if [ "$COMMAND" = "cleanup" ]; then
    TOTAL=${#TESTS[@]}
    echo "Running cleanup (destroy) on $TOTAL test(s) across $NUM_ACCOUNTS accounts"
    echo ""

    WORK_DIR=$(mktemp -d)
    trap "rm -rf $WORK_DIR; release_account_locks" EXIT

    # Pre-authenticate accounts
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
                INJECTED_FILE=$(inject_provider_source "$TEST_FILE")
                echo "RUNNING destroy $REL_PATH"
                DESTROY_OUTPUT=$(cd "$STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$INJECTED_FILE" 2>&1)
                DESTROY_RC=$?
                rm -f "$INJECTED_FILE"
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
    for PID in ${PIDS[@]+"${PIDS[@]}"}; do
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

# ── full: parallel execution across selected accounts ─────────────────
if [ "$COMMAND" = "full" ]; then
    TOTAL=${#TESTS[@]}
    echo "Running full cycle (apply -> plan-verify -> destroy) on $TOTAL test(s) across $NUM_ACCOUNTS accounts"
    if [ -n "$ACCOUNT_START" ]; then
        echo "  Using accounts: ${ACCOUNTS[*]}"
    fi
    echo ""

    WORK_DIR=$(mktemp -d)

    # Signal handling for the main process: forward signals to workers
    PIDS=()
    cleanup_main() {
        echo ""
        echo "Interrupted! Signaling workers to clean up..."
        for PID in ${PIDS[@]+"${PIDS[@]}"}; do
            kill -TERM "$PID" 2>/dev/null || true
        done
        echo "Waiting for workers to finish cleanup..."
        for PID in ${PIDS[@]+"${PIDS[@]}"}; do
            wait "$PID" 2>/dev/null || true
        done
        echo "All workers finished."
        rm -rf "$WORK_DIR"
        release_account_locks
        exit 1
    }
    trap cleanup_main INT TERM

    # Clean up temp dir and release locks on normal exit
    trap "rm -rf $WORK_DIR; release_account_locks" EXIT

    # Pre-authenticate accounts sequentially to avoid opening
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
        SLOT_DIR="$WORK_DIR/state_${SLOT}"
        mkdir -p "$SLOT_DIR"

        (
            set +e
            PASSED=0
            FAILED=0
            CURRENT_TEST_FILE=""
            CURRENT_STATE_DIR=""
            INTERRUPTED=0

            # Worker trap: attempt destroy for the current test on interruption
            worker_cleanup() {
                INTERRUPTED=1
                if [ -n "$CURRENT_TEST_FILE" ] && [ -n "$CURRENT_STATE_DIR" ]; then
                    REL_PATH="${CURRENT_TEST_FILE#$SCRIPT_DIR/}"
                    echo "INTERRUPTED - destroying resources for $REL_PATH"
                    cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$CURRENT_TEST_FILE" 2>&1 || true
                fi
            }
            trap worker_cleanup INT TERM

            TEST_INDEX=0
            while IFS= read -r TEST_FILE; do
                if [ $INTERRUPTED -eq 1 ]; then
                    break
                fi

                REL_PATH="${TEST_FILE#$SCRIPT_DIR/}"
                INJECTED_FILE=$(inject_provider_source "$TEST_FILE")
                CURRENT_TEST_FILE="$INJECTED_FILE"

                # Use a separate state directory per test to prevent
                # state file cross-contamination between tests (issue #537)
                CURRENT_STATE_DIR="$SLOT_DIR/test_${TEST_INDEX}"
                mkdir -p "$CURRENT_STATE_DIR"
                TEST_INDEX=$((TEST_INDEX + 1))

                # Apply (run from state dir so each test has its own state file)
                echo "RUNNING apply $REL_PATH"
                APPLY_OUTPUT=$(cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" apply --auto-approve "$INJECTED_FILE" 2>&1)
                APPLY_RC=$?
                if [ $INTERRUPTED -eq 1 ]; then
                    rm -f "$INJECTED_FILE"
                    break
                fi
                if [ $APPLY_RC -ne 0 ] || echo "$APPLY_OUTPUT" | grep -q "failed"; then
                    echo "FAIL (apply) $REL_PATH"
                    echo "  ERROR: $APPLY_OUTPUT"
                    FAILED=$((FAILED + 1))
                    # Try to destroy whatever was partially created
                    cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$INJECTED_FILE" 2>&1 || true
                    rm -f "$INJECTED_FILE"
                    CURRENT_TEST_FILE=""
                    CURRENT_STATE_DIR=""
                    continue
                fi

                # Post-apply plan verification (idempotency check)
                echo "RUNNING plan-verify $REL_PATH"
                PLAN_OUTPUT=$(cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" plan --detailed-exitcode "$INJECTED_FILE" 2>&1)
                PLAN_RC=$?
                if [ $INTERRUPTED -eq 1 ]; then
                    rm -f "$INJECTED_FILE"
                    break
                fi
                if [ $PLAN_RC -eq 2 ]; then
                    echo "FAIL (plan-verify) $REL_PATH"
                    echo "  ERROR: Post-apply plan detected changes (not idempotent):"
                    echo "  $PLAN_OUTPUT"
                    FAILED=$((FAILED + 1))
                    # Still destroy to clean up
                    cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$INJECTED_FILE" 2>&1 || true
                    rm -f "$INJECTED_FILE"
                    CURRENT_TEST_FILE=""
                    CURRENT_STATE_DIR=""
                    continue
                elif [ $PLAN_RC -ne 0 ]; then
                    echo "FAIL (plan-verify) $REL_PATH"
                    echo "  ERROR: $PLAN_OUTPUT"
                    FAILED=$((FAILED + 1))
                    cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$INJECTED_FILE" 2>&1 || true
                    rm -f "$INJECTED_FILE"
                    CURRENT_TEST_FILE=""
                    CURRENT_STATE_DIR=""
                    continue
                fi

                # Destroy
                echo "RUNNING destroy $REL_PATH"
                DESTROY_OUTPUT=$(cd "$CURRENT_STATE_DIR" && aws-vault exec "$ACCOUNT" -- "$CARINA_BIN" destroy --auto-approve "$INJECTED_FILE" 2>&1)
                DESTROY_RC=$?
                rm -f "$INJECTED_FILE"
                if [ $DESTROY_RC -ne 0 ] || echo "$DESTROY_OUTPUT" | grep -q "failed"; then
                    echo "FAIL (destroy) $REL_PATH"
                    echo "  ERROR: $DESTROY_OUTPUT"
                    FAILED=$((FAILED + 1))
                    CURRENT_TEST_FILE=""
                    CURRENT_STATE_DIR=""
                    continue
                fi

                echo "OK   $REL_PATH"
                PASSED=$((PASSED + 1))
                CURRENT_TEST_FILE=""
                CURRENT_STATE_DIR=""
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

# Use per-test isolated state directories for commands that touch state
# (plan/apply/destroy) to prevent cross-contamination between tests (issue #839)
if [ "$COMMAND" != "validate" ]; then
    WORK_DIR=$(mktemp -d)
    trap "rm -rf $WORK_DIR; release_account_locks" EXIT
fi

PASSED=0
FAILED=0
ERRORS=()
TEST_INDEX=0

for TEST_FILE in "${TESTS[@]}"; do
    REL_PATH="${TEST_FILE#$SCRIPT_DIR/}"
    printf "  %-55s " "$REL_PATH"

    INJECTED_FILE=$(inject_provider_source "$TEST_FILE")

    AUTO_APPROVE=""
    if [ "$COMMAND" = "apply" ] || [ "$COMMAND" = "destroy" ]; then
        AUTO_APPROVE="--auto-approve"
    fi

    # Run from an isolated state directory for stateful commands
    RUN_PREFIX=""
    if [ "$COMMAND" != "validate" ]; then
        STATE_DIR="$WORK_DIR/test_${TEST_INDEX}"
        mkdir -p "$STATE_DIR"
        RUN_PREFIX="cd $STATE_DIR &&"
        TEST_INDEX=$((TEST_INDEX + 1))
    fi

    if OUTPUT=$(eval "$RUN_PREFIX \"$CARINA_BIN\" \"$COMMAND\" $AUTO_APPROVE \"$INJECTED_FILE\"" 2>&1); then
        if [ "$COMMAND" = "apply" ]; then
            # Post-apply plan verification (idempotency check)
            # Use || to capture non-zero exit codes without triggering set -e
            PLAN_RC=0
            PLAN_OUTPUT=$(eval "$RUN_PREFIX \"$CARINA_BIN\" plan --detailed-exitcode \"$INJECTED_FILE\"" 2>&1) || PLAN_RC=$?
            if [ $PLAN_RC -eq 2 ]; then
                echo "FAIL (plan-verify)"
                ERRORS+=("$REL_PATH: Post-apply plan detected changes (not idempotent): $PLAN_OUTPUT")
                FAILED=$((FAILED + 1))
                rm -f "$INJECTED_FILE"
                continue
            elif [ $PLAN_RC -ne 0 ]; then
                echo "FAIL (plan-verify)"
                ERRORS+=("$REL_PATH: $PLAN_OUTPUT")
                FAILED=$((FAILED + 1))
                rm -f "$INJECTED_FILE"
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
    rm -f "$INJECTED_FILE"
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
