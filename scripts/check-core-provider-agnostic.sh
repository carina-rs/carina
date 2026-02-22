#!/usr/bin/env bash
# Check that carina-core remains provider-agnostic.
# Fails if any non-test, non-comment Rust source in carina-core/src/
# contains hardcoded provider-specific string literals.
set -euo pipefail

CORE_DIR="carina-core/src"

# Provider-specific patterns to reject (regex for grep -E)
# Matches string literals like "awscc.", "aws.", "awscc", "aws" used in format strings
PATTERNS='"awscc\.|"aws\.|"awscc"|"aws"'

violations=0

for file in $(find "$CORE_DIR" -name '*.rs' -type f); do
    in_test=false
    line_num=0

    while IFS= read -r line; do
        line_num=$((line_num + 1))

        # Track #[cfg(test)] mod blocks
        if echo "$line" | grep -q '#\[cfg(test)\]'; then
            in_test=true
            continue
        fi

        # Skip test code
        if $in_test; then
            continue
        fi

        # Skip comment lines
        stripped=$(echo "$line" | sed 's/^[[:space:]]*//')
        if echo "$stripped" | grep -q '^//'; then
            continue
        fi

        # Check for provider-specific patterns
        if echo "$line" | grep -qE "$PATTERNS"; then
            echo "VIOLATION: $file:$line_num: $line"
            violations=$((violations + 1))
        fi
    done < "$file"
done

if [ "$violations" -gt 0 ]; then
    echo ""
    echo "ERROR: Found $violations provider-specific string literal(s) in $CORE_DIR."
    echo "carina-core must remain provider-agnostic."
    echo "Move provider-specific logic to the appropriate provider crate."
    exit 1
fi

echo "OK: No provider-specific string literals found in $CORE_DIR (excluding tests)."
