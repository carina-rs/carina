#!/usr/bin/env bash
# Check that provider-agnostic crates do not leak provider-specific code.
#
# Enforces architectural boundaries:
# 1. carina-core Cargo.toml must not depend on provider crates
# 2. carina-core must not reference provider crates (use/inline carina_provider_*)
# 3. carina-core must not contain hardcoded provider string literals
# 4. carina-lsp must not contain hardcoded provider string literals (main.rs excluded)
# 5. carina-lsp lib code must not reference provider crates (main.rs wiring excluded)
set -euo pipefail

violations=0

# ── Helper: scan Rust files for a grep pattern, skipping tests and comments ──
# Uses bulk grep instead of per-line bash loops for performance.
check_rust_files() {
    local dir="$1"
    local pattern="$2"
    local description="$3"
    local exclude_files="${4:-}"

    local find_args=("$dir" -name '*.rs' -type f)

    # Exclude test-only files: *_tests.rs, tests.rs, and files under tests/ directories
    find_args+=(-not -name '*_tests.rs' -not -name 'tests.rs' -not -path '*/tests/*')

    # Build -not -name args for excluded files
    if [ -n "$exclude_files" ]; then
        for excl in $exclude_files; do
            find_args+=(-not -name "$excl")
        done
    fi

    # For each file: grep with line numbers, then filter out test blocks and comments.
    # This preserves correct line numbers unlike sed-then-grep.
    while IFS= read -r file; do
        # Find where #[cfg(test)] starts (if any)
        local test_line
        test_line=$(grep -n '#\[cfg(test)\]' "$file" 2>/dev/null | head -1 | cut -d: -f1) || true

        # grep with line numbers
        local matches
        matches=$(grep -nE "$pattern" "$file" 2>/dev/null) || continue

        while IFS= read -r match; do
            local line_num="${match%%:*}"
            local line_content="${match#*:}"

            # Skip if in test block
            if [ -n "${test_line:-}" ] && [ "$line_num" -ge "$test_line" ]; then
                continue
            fi

            # Skip comment lines
            local stripped="${line_content#"${line_content%%[![:space:]]*}"}"
            if [[ "$stripped" == //* ]]; then
                continue
            fi

            echo "VIOLATION ($description): $file:$line_num: $line_content"
            violations=$((violations + 1))
        done <<< "$matches"
    done < <(find "${find_args[@]}")
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
        if grep -qE "^$dep( |=)" "$toml" 2>/dev/null; then
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

# ── Check 2: carina-core must not reference provider crates ───────────────────
echo "=== Check 2: carina-core provider references ==="
check_rust_files "carina-core/src" \
    'carina_provider_' \
    "carina-core provider reference"
echo "  Done."

# ── Check 3: carina-core must not contain provider string literals ───────────
echo "=== Check 3: carina-core provider string literals ==="
check_rust_files "carina-core/src" \
    '"awscc\.|"aws\.|"awscc"|"aws"' \
    "carina-core provider string"
echo "  Done."

# ── Check 4: carina-lsp must not contain provider string literals ──────────────
echo "=== Check 4: carina-lsp provider string literals ==="
check_rust_files "carina-lsp/src" \
    '"awscc\.|"aws\.|"awscc"|"aws"' \
    "carina-lsp provider string" \
    "main.rs"
echo "  Done."

# ── Check 5: carina-lsp lib must not reference provider crates ────────────────
# main.rs is excluded because it is the wiring entry point that legitimately
# instantiates provider factories.
echo "=== Check 5: carina-lsp provider references (excluding main.rs wiring) ==="
check_rust_files "carina-lsp/src" \
    'carina_provider_' \
    "carina-lsp provider reference" \
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
