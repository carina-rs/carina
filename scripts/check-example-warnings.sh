#!/bin/bash
# Check that example .crn files produce no warnings when validated.
#
# The `carina validate` command exits 0 even when warnings are emitted,
# so this script inspects the output text for warning indicators.
#
# Usage (from project root):
#   ./scripts/check-example-warnings.sh

set -euo pipefail

DIRS=(
    "carina-provider-aws/examples"
    "carina-provider-awscc/examples"
)

# Build the CLI binary
cargo build --bin carina --quiet || {
    echo "ERROR: Could not build carina binary"
    exit 1
}

CARINA="target/debug/carina"
WARNINGS=0
PASSED=0

for dir in "${DIRS[@]}"; do
    if [ ! -d "$dir" ]; then
        echo "SKIP: $dir (not found)"
        continue
    fi

    while IFS= read -r -d '' file; do
        output=$("$CARINA" validate "$file" 2>&1) || {
            echo "ERROR: $file (non-zero exit)"
            echo "$output"
            echo ""
            WARNINGS=$((WARNINGS + 1))
            continue
        }

        # Check for warning indicators in output
        if echo "$output" | grep -qE '⚠|^warning:'; then
            echo "WARNING: $file"
            echo "$output" | grep -E '⚠|^warning:'
            echo ""
            WARNINGS=$((WARNINGS + 1))
        else
            PASSED=$((PASSED + 1))
        fi
    done < <(find "$dir" -name '*.crn' -print0)
done

echo ""
echo "Results: $PASSED passed, $WARNINGS with warnings"

if [ "$WARNINGS" -gt 0 ]; then
    echo ""
    echo "Fix the warnings above before merging."
    echo "Common fixes:"
    echo "  - Unused let binding: remove 'let <name> =' and use anonymous resource"
    exit 1
fi

echo "All example .crn files are warning-free."
