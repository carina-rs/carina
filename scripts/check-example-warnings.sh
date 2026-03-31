#!/bin/bash
# Check that example and acceptance test .crn files produce no warnings when validated.
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
    "carina-provider-aws/acceptance-tests"
    "carina-provider-awscc/acceptance-tests"
)

# Files that require runtime environment (env vars, KMS) and cannot be validated in CI
SKIP_FILES=(
    "carina-provider-awscc/acceptance-tests/secret_env/decrypt_tag.crn"
    "carina-provider-awscc/acceptance-tests/secret_env/env_tag.crn"
    "carina-provider-awscc/acceptance-tests/secret_env/secret_tag.crn"
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
        # Skip files with unavoidable warnings
        skip=false
        for skip_file in "${SKIP_FILES[@]}"; do
            if [ "$file" = "$skip_file" ]; then
                skip=true
                break
            fi
        done
        if $skip; then
            PASSED=$((PASSED + 1))
            continue
        fi

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

echo "All .crn files are warning-free."
