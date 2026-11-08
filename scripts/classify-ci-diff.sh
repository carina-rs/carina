#!/usr/bin/env bash
# Classify committed file contents, never release branch names or labels.
# Python 3.11+ supplies the TOML parser; unavailable tools/errors fail closed.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo full
  exit 0
fi

if ! classification=$(python3 - "$@" <<'PY'
import re
import subprocess
import sys
import tomllib


def git(*args):
    return subprocess.check_output(
        ["git", *args], stderr=subprocess.DEVNULL, timeout=30,
    )


VERSION = re.compile(r'([ \t]*version[ \t]*=[ \t]*)"([^"\\\r\n]*)"([ \t]*(?:#[^\r\n]*)?)(\r?\n|$)')
PACKAGE = re.compile(r'\[\s*package\s*\]\s*(?:#.*)?')
WORKSPACE_PACKAGE = re.compile(r'\[\s*workspace\s*\.\s*package\s*\]\s*(?:#.*)?')
LOCK_PACKAGE = re.compile(r'\[\[\s*package\s*\]\]\s*(?:#.*)?')
DOCS = {
    "site/src/content/providers/writing/overview.md",
    "site/src/content/providers/writing/versioning.md",
}


def normalize_versions(text, lockfile):
    # Parse the entire file, including source fields outside the diff's hunks.
    # Comparing parsed data as well as normalized lines also prevents a fake
    # table/version inside a multiline string from hiding an unrelated edit.
    data = tomllib.loads(text)
    if lockfile:
        tables = data["package"]
        if not isinstance(tables, list):
            raise ValueError("invalid lockfile packages")
    else:
        tables = [data.get("package", {}), data.get("workspace", {}).get("package", {})]
    versions = {}
    for index, table in enumerate(tables):
        if not isinstance(table, dict):
            raise ValueError("invalid package table")
        if (not lockfile or "source" not in table) and isinstance(table.get("version"), str):
            versions[index] = table.pop("version")

    section = None
    package_index = -1
    normalized = []
    for line in text.splitlines(keepends=True):
        header = line.strip()
        if header.startswith("["):
            section = None
            if lockfile and LOCK_PACKAGE.fullmatch(header):
                package_index += 1
                section = package_index
            elif not lockfile and PACKAGE.fullmatch(header):
                section = 0
            elif not lockfile and WORKSPACE_PACKAGE.fullmatch(header):
                section = 1
        match = VERSION.fullmatch(line)
        if match and section in versions and match[2] == versions[section]:
            line = match[1] + '"__ci_version__"' + match[3] + match[4]
        normalized.append(line)
    return "".join(normalized), data


def classify(base_ref, head_ref):
    base, head = (
        git("rev-parse", "--verify", "--end-of-options", ref + "^{commit}").strip().decode("ascii")
        for ref in (base_ref, head_ref)
    )
    options = [
        "--no-renames", "--no-ext-diff", "--no-textconv",
        "--ignore-submodules=none", "-z", base, head, "--",
    ]
    raw = git("diff", "--raw", "--no-abbrev", *options).split(b"\0")
    if raw.pop() != b"" or not raw or len(raw) % 2:
        return "full"
    # numstat distinguishes binary diffs, including attributes marking text as
    # binary. --no-renames turns every rename into a rejected add/delete pair.
    stats = git("diff", "--numstat", *options).split(b"\0")
    if stats.pop() != b"":
        return "full"
    text_paths = set()
    for stat in stats:
        added, removed, path = stat.split(b"\t", 2)
        if not added.isdigit() or not removed.isdigit():
            return "full"
        text_paths.add(path)
    if text_paths != set(raw[1::2]):
        return "full"

    for metadata, path_bytes in zip(raw[::2], raw[1::2]):
        match = re.fullmatch(rb":(100644|100755) (100644|100755) ([0-9a-f]+) ([0-9a-f]+) M", metadata)
        if not match or match[1] != match[2]:
            return "full"
        path = path_bytes.decode("utf-8")
        if path not in DOCS and path != "Cargo.lock" and path.split("/")[-1] != "Cargo.toml":
            return "full"
        contents = [git("cat-file", "blob", oid.decode("ascii")) for oid in (match[3], match[4])]
        if any(b"\0" in content for content in contents):
            return "full"
        before, after = (content.decode("utf-8") for content in contents)
        if path not in DOCS and normalize_versions(before, path == "Cargo.lock") != normalize_versions(after, path == "Cargo.lock"):
            return "full"
    return "version-only"


try:
    print(classify(*sys.argv[1:]))
except Exception:
    # Git errors, missing objects, malformed TOML, and unexpected formats must
    # never authorize skipping the full suite.
    print("full")
PY
); then
  classification=full
fi

case "$classification" in
  version-only|full) printf '%s\n' "$classification" ;;
  *) echo full ;;
esac
