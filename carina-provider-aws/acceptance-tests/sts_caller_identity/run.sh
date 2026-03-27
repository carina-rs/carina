#!/bin/bash
# Runs sts_caller_identity acceptance tests
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FILTER="${1:-}"

TOTAL_PASSED=0
TOTAL_FAILED=0
TESTS_RUN=0

echo "sts_caller_identity acceptance tests"
echo "════════════════════════════════════════"

for test_file in "$SCRIPT_DIR"/tests/*.sh; do
    [ "$(basename "$test_file")" = "_helpers.sh" ] && continue
    test_name="$(basename "$test_file" .sh)"
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

[ "$TOTAL_FAILED" -gt 0 ] && exit 1
