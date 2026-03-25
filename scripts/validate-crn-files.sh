#!/bin/bash
# Validate all .crn files in acceptance-tests and examples directories.
#
# This runs `carina validate` on each .crn file to catch parse/validation
# errors without requiring AWS credentials.
#
# Usage (from project root):
#   ./scripts/validate-crn-files.sh

set -e

DIRS=(
    "carina-provider-aws/acceptance-tests"
    "carina-provider-aws/examples"
    "carina-provider-awscc/acceptance-tests"
    "carina-provider-awscc/examples"
)

# Build the CLI binary
cargo build --bin carina --quiet || {
    echo "ERROR: Could not build carina binary"
    exit 1
}

CARINA="target/debug/carina"
FAILED=0
PASSED=0

for dir in "${DIRS[@]}"; do
    if [ ! -d "$dir" ]; then
        echo "SKIP: $dir (not found)"
        continue
    fi

    while IFS= read -r -d '' file; do
        if "$CARINA" validate "$file" > /dev/null 2>&1; then
            PASSED=$((PASSED + 1))
        else
            echo "FAIL: $file"
            # Show the error output for debugging
            "$CARINA" validate "$file" 2>&1 || true
            echo ""
            FAILED=$((FAILED + 1))
        fi
    done < <(find "$dir" -name '*.crn' -print0)
done

echo ""
echo "Results: $PASSED passed, $FAILED failed"

if [ "$FAILED" -gt 0 ]; then
    exit 1
fi

echo "All .crn files validated successfully."
