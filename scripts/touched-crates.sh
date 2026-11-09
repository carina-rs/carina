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
# A cargo metadata failure warns and deliberately widens to `--workspace`
# to avoid missing tests. Git failures are errors; failure to discover changed
# files must not look like no changes.
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
#   5. Otherwise-ignorable files outside crates are owned by crates whose
#      tracked Rust sources contain a path string literal resolving to the
#      tracked or changed file or an ancestor directory. Scan doc comments,
#      excluding ordinary comments and bare names; resolve relative to the
#      source directory, crate root and repo root, stripping one leading /.
#      Include reverse dependencies; a carina-core owner means --workspace.

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
    if ! committed_files=$(git diff --no-renames --name-only "$base_ref"...HEAD); then
        echo "touched-crates.sh: git diff $base_ref...HEAD failed; cannot determine touched crates" >&2
        exit 1
    fi
    if ! working_files=$(git diff --no-renames --name-only HEAD); then
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

    candidates+=("$f")
done

if [[ ${#candidates[@]} -eq 0 ]]; then
    # Discovery may have returned only blank lines.
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
import os.path
from pathlib import Path
import re
import subprocess
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

def owner_of(path):
    return next((parent for parent in (path, *path.parents) if parent in packages), None)

def is_ignorable(filename):
    path = Path(filename)
    # Preserve main touched-crates ignores (including any-depth *.md), plus
    # known non-code paths excluded by main ci.yml changes regex. Unknown
    # paths still require the workspace; verification plumbing is handled above.
    return (
        filename in {".gitignore", ".gitmodules", "Makefile", "release.toml", "clippy.toml"}
        or (len(path.parts) == 1 and filename.startswith("LICENSE"))
        or path.parts[0] in {".github", "scripts", "site", "notes", "infra", "examples",
                             "packaging", "assets", "editors", "docs"}
        or filename.endswith(".md")
    )

# Recognize whole strings before looking for comments within their contents.
# Raw strings have a variable delimiter; character literals must also consume
# quotes so e.g. a double-quote character cannot start a string token.
rust_tokens = re.compile(
    r"(?P<line_doc>(?:///(?!/)|//!)[^\n]*(?:\n[ \t]*(?:///(?!/)|//!)[^\n]*)*)"
    r"|//[^\n]*|/\*|(?:br|cr|r)(?P<hashes>#{0,255})\x22"
    r"|\x22(?P<quoted>(?:[^\x22\\]|\\.)*)\x22"
    r"|\x27(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^\\\x27\r\n])\x27",
    re.DOTALL,
)
block_comments = re.compile(r"/\*|\*/")
rust_escapes = re.compile(r"\\(?:\r?\n[ \t\r\n]*|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)", re.DOTALL)

def unescape(match):
    value = match[0][1:]
    if value.startswith(("\n", "\r")):
        return ""
    if value.startswith("x"):
        return chr(int(value[1:], 16))
    if value.startswith("u{"):
        return chr(int(value[2:-1].replace("_", ""), 16))
    return {"n": "\n", "r": "\r", "t": "\t", "0": "\0", "\\": "\\",
            "\x22": "\x22", "\x27": "\x27"}[value]

def string_literals(text):
    position = 0
    while match := rust_tokens.search(text, position):
        position = match.end()
        token = match[0]
        if match["line_doc"] is not None:
            # Keep adjacent doc lines together so multiline Rust strings and
            # ordinary comments inside doctests retain their token boundaries.
            body = "\n".join(line.lstrip(" \t")[3:] for line in token.splitlines())
            yield from string_literals(body)
            continue
        if token.startswith("//") or token.startswith("\x27"):
            continue
        if token == "/*":
            is_doc = (text.startswith(("/**", "/*!"), match.start())
                      and not text.startswith("/***", match.start()))
            body_start = position + 1
            depth = 1
            while depth:
                comment = block_comments.search(text, position)
                if comment is None:
                    return
                depth += 1 if comment[0] == "/*" else -1
                position = comment.end()
            if is_doc:
                yield from string_literals(text[body_start:position - 2])
        elif match["hashes"] is not None:
            delimiter = "\x22" + match["hashes"]
            end = text.find(delimiter, position)
            if end == -1:
                return
            yield text[position:end]
            position = end + len(delimiter)
        else:
            yield rust_escapes.sub(unescape, match["quoted"])

def reference_owners():
    # One index query and one scan, independent of the number of changed files.
    tracked = subprocess.run(["git", "ls-files", "-z"], cwd=root,
                             stdout=subprocess.PIPE, check=True, timeout=30)
    files = {name for name in os.fsdecode(tracked.stdout).split("\0") if name}
    # Deleted and untracked inputs may be absent from the index. Their paths
    # and ancestors must still match references in the remaining Rust sources.
    candidate_names = files | set(sys.argv[1:])
    paths = {str(root / name) for name in candidate_names}
    for name in candidate_names:
        paths.update(str(root / parent) for parent in Path(name).parents if parent != Path("."))

    owners = {}
    for name in sorted(name for name in files if name.endswith(".rs")):
        source = root / name
        owner = owner_of(source)
        if owner is None:
            continue
        try:
            text = source.read_text()
        except FileNotFoundError:
            # A tracked source may have been deleted in the working tree.
            continue
        for literal in set(string_literals(text)):
            if "/" not in literal:
                continue
            # concat!(env!("CARGO_MANIFEST_DIR"), "/../...") has a slash
            # prefix that must not discard the base passed to os.path.join.
            literal = literal.removeprefix("/")
            for base in (source.parent, owner, root):
                resolved = os.path.normpath(os.path.join(base, literal))
                if resolved in paths:
                    owners.setdefault(resolved, set()).add(owner)
    return owners

references = reference_owners()

touched = set()
for filename in sys.argv[1:]:
    path = (root / filename).resolve()
    owner = owner_of(path)
    if owner is not None:
        touched.add(owner)
    elif is_ignorable(filename):
        for referenced in (path, *path.parents):
            touched.update(references.get(str(referenced), ()))
    else:
        print("--workspace")
        sys.exit(0)

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
if len(touched) == len(packages) or any(packages[path]["name"] == "carina-core" for path in touched):
    print("--workspace")
else:
    print(" ".join("-p " + name for name in sorted(packages[path]["name"] for path in touched)))
' "${candidates[@]}" <<< "$metadata"); then
    echo 'touched-crates.sh: cannot derive crate scope; using --workspace' >&2
    echo '--workspace'
    exit 0
fi
echo "$selection"
