#!/usr/bin/env bash
# Check that every fixture directory under plan_display/ has a corresponding
# Makefile target that references it.
set -euo pipefail

FIXTURES_DIR="carina-cli/tests/fixtures/plan_display"
MAKEFILE="Makefile"

missing=0

for dir in "$FIXTURES_DIR"/*/; do
    name="$(basename "$dir")"
    if ! grep -q "$FIXTURES_DIR/$name\b\|/\$name " "$MAKEFILE" 2>/dev/null; then
        # Check if the directory name appears in any FIXTURES-relative path in the Makefile
        if ! grep -q "/$name " "$MAKEFILE" 2>/dev/null && ! grep -q "/$name && " "$MAKEFILE" 2>/dev/null; then
            echo "MISSING: fixture directory '$name' has no corresponding Makefile target"
            missing=$((missing + 1))
        fi
    fi
done

echo ""
if [ "$missing" -gt 0 ]; then
    echo "ERROR: Found $missing fixture directory(ies) without Makefile targets."
    echo "Add a 'plan-*' target in Makefile for each fixture directory."
    exit 1
fi

echo "OK: All plan_display fixture directories have Makefile targets."
