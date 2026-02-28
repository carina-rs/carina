#!/usr/bin/env bash
# Check that provider-agnostic crates do not leak provider-specific code.
#
# Enforces architectural boundaries:
# 1. carina-core Cargo.toml must not depend on provider crates
# 2. carina-core must not import provider crates (use carina_provider_*)
# 3. carina-core must not contain hardcoded provider string literals
# 4. carina-lsp lib code must not import provider crates (main.rs wiring excluded)
set -euo pipefail

violations=0

# ── Helper: scan Rust files for a grep pattern, skipping tests and comments ──
check_rust_files() {
    local dir="$1"
    local pattern="$2"
    local description="$3"
    local exclude_files="${4:-}"

    for file in $(find "$dir" -name '*.rs' -type f); do
        # Skip excluded files
        if [ -n "$exclude_files" ]; then
            local basename
            basename=$(basename "$file")
            if echo "$exclude_files" | grep -qw "$basename"; then
                continue
            fi
        fi

        local in_test=false
        local line_num=0

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
            local stripped
            stripped=$(echo "$line" | sed 's/^[[:space:]]*//')
            if echo "$stripped" | grep -q '^//'; then
                continue
            fi

            # Check for pattern
            if echo "$line" | grep -qE "$pattern"; then
                echo "VIOLATION ($description): $file:$line_num: $line"
                violations=$((violations + 1))
            fi
        done < "$file"
    done
}

# ── Check 1: Cargo.toml dependency check ─────────────────────────────────────
echo "=== Check 1: Cargo.toml dependency boundaries ==="

for crate in carina-core carina-lsp; do
    toml="$crate/Cargo.toml"
    if [ ! -f "$toml" ]; then
        echo "WARNING: $toml not found, skipping"
        continue
    fi

    for dep in carina-provider-aws carina-provider-awscc; do
        if grep -q "^$dep\b\|^$dep " "$toml" 2>/dev/null; then
            # For carina-lsp, provider deps in Cargo.toml are allowed
            # because main.rs (wiring) needs them
            if [ "$crate" = "carina-lsp" ]; then
                continue
            fi
            echo "VIOLATION (Cargo.toml dep): $toml depends on $dep"
            violations=$((violations + 1))
        fi
    done
done

echo "  Done."

# ── Check 2: carina-core must not import provider crates ─────────────────────
echo "=== Check 2: carina-core provider imports ==="
check_rust_files "carina-core/src" \
    'use carina_provider_' \
    "carina-core provider import"
echo "  Done."

# ── Check 3: carina-core must not contain provider string literals ───────────
echo "=== Check 3: carina-core provider string literals ==="
check_rust_files "carina-core/src" \
    '"awscc\.|"aws\.|"awscc"|"aws"' \
    "carina-core provider string"
echo "  Done."

# ── Check 4: carina-lsp lib must not import provider crates ──────────────────
# main.rs is excluded because it is the wiring entry point that legitimately
# instantiates provider factories.
echo "=== Check 4: carina-lsp provider imports (excluding main.rs wiring) ==="
check_rust_files "carina-lsp/src" \
    'use carina_provider_' \
    "carina-lsp provider import" \
    "main.rs"
echo "  Done."

# ── Result ────────────────────────────────────────────────────────────────────
echo ""
if [ "$violations" -gt 0 ]; then
    echo "ERROR: Found $violations provider boundary violation(s)."
    echo "Provider-agnostic crates must not contain provider-specific code."
    echo "Move provider-specific logic to the appropriate provider crate."
    exit 1
fi

echo "OK: All provider boundary checks passed."
