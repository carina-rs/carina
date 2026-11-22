#!/usr/bin/env bash
# Keep the crate-visible doctest root in sync with the production module tree.
# An optional repository root supports isolated, cargo-free fixtures.
set -euo pipefail
repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
python3 - "$repo_root" <<'PY'
from pathlib import Path
import difflib
import re
import sys

root = Path(sys.argv[1])
lib = "carina-core/src/lib.rs"
mirror = "carina-core/src/internal_rule_input_doctest.rs"
guidance = (
    "carina-core/src/internal_rule_input_doctest.rs mirrors lib.rs's module tree "
    "for crate-visible compile_fail doctests; add the same declaration there "
    "(same cfg and visibility) so the doctest crate compiles the same production modules."
)
# Exact declarations prevent an exception from hiding changed cfg/visibility.
lib_only = {
    "#[cfg(test)] pub(crate) mod test_support {":
        "Inline libtest helpers; the included doctest crate is a binary, not a cfg(test) library.",
}


def declarations(path):
    # Root headers use rustfmt's unindented form. Retain all outer attributes,
    # including multiline cfgs; ignore re-exports and indented child modules.
    modules = []
    attributes = []
    attribute = []
    for line in path.read_text().splitlines():
        if line.startswith("///") and not line.startswith("////"):
            attributes.append(line)
            continue
        if not line.strip() or line.lstrip().startswith("//"):
            continue
        if line.startswith("#[") or attribute:
            attribute.append(line.strip())
            if sum(part.count("[") - part.count("]") for part in attribute) == 0:
                attributes.append(" ".join(attribute))
                attribute = []
            continue
        module = re.match(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+[^\s;{]+\s*[;{]", line)
        if module:
            modules.append(" ".join([*attributes, module[0].strip()]))
        attributes = []
    if attribute:
        raise ValueError(f"{path}: unterminated module attribute")
    return modules


try:
    production = declarations(root / lib)
    copied = declarations(root / mirror)
    stale_exceptions = False
    for declaration, reason in lib_only.items():
        if production.count(declaration) != 1:
            print(f"ERROR: stale lib-only exception: {declaration} ({reason})", file=sys.stderr)
            stale_exceptions = True
        else:
            production.remove(declaration)
    if stale_exceptions or production != copied:
        print("ERROR: doctest module declarations differ:", file=sys.stderr)
        sys.stderr.writelines(difflib.unified_diff(
            [line + "\n" for line in production], [line + "\n" for line in copied],
            fromfile=lib, tofile=mirror,
        ))
        raise ValueError(guidance)
except (OSError, ValueError) as error:
    print(f"ERROR: {error}", file=sys.stderr)
    if str(error) != guidance:
        print(guidance, file=sys.stderr)
    sys.exit(1)
print("OK: crate-visible doctest module declarations match lib.rs (cfg, visibility and order).")
PY
