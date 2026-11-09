#!/usr/bin/env bash
# touched-crates.sh — emit cargo `-p <crate>` flags for the workspace
# crates affected by a set of changed files.
#
# Usage:
#   scripts/touched-crates.sh                  # diff against origin/main
#   scripts/touched-crates.sh --base <ref>     # diff against <ref>
#   scripts/touched-crates.sh --diff           # short for --base origin/main
#   git diff --name-only main | scripts/touched-crates.sh --stdin
# Diff mode includes committed, staged, unstaged and untracked changes.
#
# Output (stdout):
#   "-p carina-cli -p carina-tui"    # touched crates and reverse dependents
#   "--workspace"                    # cross-cutting change, fall back
#   ""                               # nothing test-relevant changed
#
# The script never fails on classification — when in doubt, it emits
# `--workspace` rather than under-test the change.
# Failure to discover changed files is fatal; it must not look like no changes.
#
# Heuristics (in order):
#   1. A change to any `Cargo.toml` or `Cargo.lock` ⇒ `--workspace`.
#   2. A change to `carina-core/**` ⇒ `--workspace` (everything depends
#      on carina-core; running its tests alone is insufficient).
#   3. WASM fixture crates and files defining Cargo check execution
#      (verify/test setup, .config, .cargo, CI and local actions) ⇒ `--workspace`.
#      Artifact consumers have no Cargo dependency edge to their fixtures.
#   4. Otherwise, find the workspace members owning the changed files
#      using Cargo metadata. Include all transitive reverse dependencies,
#      including dev, build, optional and target-specific dependencies.
#      If any changed file is outside a workspace crate
#      AND outside the "ignore" set (site/notes/other CI/scripts/infra), emit
#      `--workspace` to be safe.
#   5. Remaining files in the ignore set (CLAUDE.md, .github/**, scripts/**,
#      site/**, notes/**, infra/**, README.md, .gitignore, *.md at root) do
#      not trigger any test crates on their own.

set -euo pipefail

mode="diff"
base_ref="origin/main"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --base)
            if [[ $# -lt 2 || -z "$2" || "$2" == -* ]]; then
                echo 'touched-crates.sh: --base requires a ref' >&2
                exit 2
            fi
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
    # Process substitution hides command failures from mapfile and errexit.
    # Check each discovery command before classifying any of its output.
    if ! committed_files=$(git diff --name-only "$base_ref"...HEAD); then
        echo "touched-crates.sh: git diff $base_ref...HEAD failed; cannot determine touched crates" >&2
        exit 1
    fi
    if ! working_files=$(git diff --name-only HEAD); then
        echo 'touched-crates.sh: git diff HEAD failed; cannot determine touched crates' >&2
        exit 1
    fi
    if ! untracked_files=$(git ls-files --others --exclude-standard); then
        echo 'touched-crates.sh: git ls-files failed; cannot determine touched crates' >&2
        exit 1
    fi
    mapfile -t files < <(printf '%s\n' "$committed_files" "$working_files" "$untracked_files" | sort -u)
fi

if [[ ${#files[@]} -eq 0 ]]; then
    exit 0
fi

# Keep artifact scope in sync with verify.sh's wasm-fixtures build.
mapfile -t fixture_crates < "$(dirname "${BASH_SOURCE[0]}")/wasm-fixture-crates.txt"

is_ignorable() {
    local f="$1"
    case "$f" in
        CLAUDE.md|README.md|.gitignore|.gitmodules) return 0 ;;
        .github/*|scripts/*|site/*|notes/*|infra/*|examples/*) return 0 ;;
        *.md) [[ "$f" != */* ]] && return 0 ;;  # other top-level docs
    esac
    return 1
}

candidates=()
for f in "${files[@]}"; do
    [[ -z "$f" ]] && continue

    case "$f" in
        Cargo.toml|Cargo.lock|*/Cargo.toml|*/Cargo.lock|carina-core|carina-core/*)
            echo '--workspace'
            exit 0
            ;;
        scripts/verify.sh|scripts/touched-crates.sh|scripts/test-in-process-isolation.sh|\
        scripts/setup-nextest-user-dirs.sh|scripts/require-wasm-fixtures.sh|scripts/wasm-fixture-crates.txt|\
        .config/*|.cargo/*|.github/workflows/ci.yml|.github/actions/*)
            echo '--workspace'
            exit 0
            ;;
    esac

    for fixture in "${fixture_crates[@]}"; do
        case "$f" in
            "$fixture"|"$fixture"/*)
                echo '--workspace'
                exit 0
                ;;
        esac
    done

    # Static scripts run regardless of crate scope; docs need no Cargo check.
    if is_ignorable "$f"; then
        continue
    fi

    candidates+=("$f")
done

if [[ ${#candidates[@]} -eq 0 ]]; then
    # Only ignorable changes — nothing to test.
    exit 0
fi

# --no-deps still includes every member's declared dependencies, across all
# kinds and targets. Match their paths rather than names (which can be renamed).
if ! metadata=$(cargo metadata --format-version 1 --no-deps); then
    echo 'touched-crates.sh: cargo metadata failed; using --workspace' >&2
    echo '--workspace'
    exit 0
fi
if ! selection=$(python3 -c '
import json
from pathlib import Path
import sys

metadata = json.load(sys.stdin)
members = set(metadata["workspace_members"])
packages = {
    Path(package["manifest_path"]).resolve().parent: package
    for package in metadata["packages"] if package["id"] in members
}
if not packages or len(packages) != len(members):
    raise ValueError("incomplete workspace metadata")
root = Path(metadata["workspace_root"]).resolve()
touched = set()
for filename in sys.argv[1:]:
    path = (root / filename).resolve()
    owner = next((parent for parent in (path, *path.parents) if parent in packages), None)
    if owner is None:
        print("--workspace")
        sys.exit(0)
    touched.add(owner)

dependents = {directory: set() for directory in packages}
for directory, package in packages.items():
    for dependency in package["dependencies"]:
        if dependency.get("path") is not None:
            target = Path(dependency["path"]).resolve()
            if target in dependents:
                dependents[target].add(directory)

pending = list(touched)
while pending:
    for dependent in dependents[pending.pop()] - touched:
        touched.add(dependent)
        pending.append(dependent)
if len(touched) == len(packages):
    print("--workspace")
else:
    print(" ".join("-p " + name for name in sorted(packages[path]["name"] for path in touched)))
' "${candidates[@]}" <<< "$metadata"); then
    echo 'touched-crates.sh: cannot derive crate scope; using --workspace' >&2
    echo '--workspace'
    exit 0
fi
echo "$selection"
