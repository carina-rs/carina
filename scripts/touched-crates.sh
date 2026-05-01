#!/usr/bin/env bash
# touched-crates.sh — emit cargo `-p <crate>` flags for the workspace
# crates affected by a set of changed files.
#
# Usage:
#   scripts/touched-crates.sh                  # diff against origin/main
#   scripts/touched-crates.sh --base <ref>     # diff against <ref>
#   scripts/touched-crates.sh --diff           # short for --base origin/main
#   git diff --name-only main | scripts/touched-crates.sh --stdin
#
# Output (stdout):
#   "-p carina-core -p carina-cli"   # one or more touched crates
#   "--workspace"                    # cross-cutting change, fall back
#   ""                               # nothing test-relevant changed
#
# The script never fails on classification — when in doubt, it emits
# `--workspace` rather than under-test the change.
#
# Heuristics (in order):
#   1. A change to root `Cargo.toml`, `Cargo.lock`, or any
#      `*/Cargo.toml` of a depended-upon crate ⇒ `--workspace`.
#   2. A change to `carina-core/**` ⇒ `--workspace` (everything depends
#      on carina-core; running its tests alone is insufficient).
#   3. Otherwise, collect the leading `carina-*/` segment from each
#      changed file. If any changed file is outside a workspace crate
#      AND outside the "ignore" set (docs/CI/scripts/infra), emit
#      `--workspace` to be safe.
#   4. Files in the ignore set (CLAUDE.md, .github/**, scripts/**,
#      docs/**, infra/**, README.md, .gitignore, *.md at root) do not
#      trigger any test crates on their own.

set -euo pipefail

mode="diff"
base_ref="origin/main"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --base)
            base_ref="$2"
            shift 2
            ;;
        --diff)
            mode="diff"
            shift
            ;;
        --stdin)
            mode="stdin"
            shift
            ;;
        -h|--help)
            sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's|^# \?||'
            exit 0
            ;;
        *)
            echo "touched-crates.sh: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [[ "$mode" == "stdin" ]]; then
    mapfile -t files < <(cat)
else
    if ! git rev-parse --verify "$base_ref" >/dev/null 2>&1; then
        echo "touched-crates.sh: base ref $base_ref does not exist" >&2
        exit 2
    fi
    mapfile -t files < <(git diff --name-only "$base_ref"...HEAD)
fi

if [[ ${#files[@]} -eq 0 ]]; then
    exit 0
fi

# Workspace crate dirs (matches Cargo.toml [workspace] members).
crates=(
    carina-cli
    carina-core
    carina-lsp
    carina-plugin-host
    carina-plugin-sdk
    carina-provider-mock
    carina-provider-protocol
    carina-provider-resolver
    carina-state
    carina-tui
)

is_crate_dir() {
    local seg="$1"
    for c in "${crates[@]}"; do
        [[ "$c" == "$seg" ]] && return 0
    done
    return 1
}

is_ignorable() {
    local f="$1"
    case "$f" in
        CLAUDE.md|README.md|.gitignore|.gitmodules) return 0 ;;
        .github/*|scripts/*|docs/*|infra/*|examples/*) return 0 ;;
        *.md) return 0 ;;  # other top-level docs
    esac
    return 1
}

declare -A touched
touched_count=0
fallback=0

for f in "${files[@]}"; do
    [[ -z "$f" ]] && continue

    # Heuristic 1: workspace-level Cargo files
    if [[ "$f" == "Cargo.toml" || "$f" == "Cargo.lock" ]]; then
        fallback=1
        break
    fi

    seg="${f%%/*}"

    # Heuristic 2: carina-core touched ⇒ workspace
    if [[ "$seg" == "carina-core" ]]; then
        fallback=1
        break
    fi

    # Workspace crate?
    if is_crate_dir "$seg"; then
        if [[ -z "${touched[$seg]:-}" ]]; then
            touched["$seg"]=1
            touched_count=$((touched_count + 1))
        fi
        continue
    fi

    # Ignorable docs/CI/script change?
    if is_ignorable "$f"; then
        continue
    fi

    # Anything else (a path we do not recognize): be safe.
    fallback=1
    break
done

if [[ "$fallback" -eq 1 ]]; then
    echo "--workspace"
    exit 0
fi

if [[ "$touched_count" -eq 0 ]]; then
    # Only ignorable changes — nothing to test.
    exit 0
fi

# Emit `-p <crate>` for each touched crate, sorted for determinism.
out=""
for c in $(printf '%s\n' "${!touched[@]}" | sort); do
    out+=" -p $c"
done
echo "${out# }"
