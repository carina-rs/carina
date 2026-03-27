#!/bin/bash
# Runs all if_expression acceptance tests
#
# Usage:
#   aws-vault exec <profile> -- ./run.sh [filter]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FILTER="${1:-}"

TOTAL_PASSED=0
TOTAL_FAILED=0
TESTS_RUN=0

echo "if_expression acceptance tests"
echo "════════════════════════════════════════"

for test_file in "$SCRIPT_DIR"/tests/*.sh; do
    # Skip the helpers file
    [ "$(basename "$test_file")" = "_helpers.sh" ] && continue

    test_name="$(basename "$test_file" .sh)"

    # Apply filter if provided
    if [ -n "$FILTER" ] && ! echo "$test_name" | grep -q "$FILTER"; then
        continue
    fi

    echo ""
    TESTS_RUN=$((TESTS_RUN + 1))

    if bash "$test_file"; then
        TOTAL_PASSED=$((TOTAL_PASSED + 1))
    else
        TOTAL_FAILED=$((TOTAL_FAILED + 1))
    fi
done

echo ""
echo "════════════════════════════════════════"
echo "Tests run: $TESTS_RUN, $TOTAL_PASSED passed, $TOTAL_FAILED failed"
echo "════════════════════════════════════════"

if [ "$TOTAL_FAILED" -gt 0 ]; then
    exit 1
fi
