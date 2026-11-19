#!/usr/bin/env python3
"""Keep binding decomposition and child minting at their core-owned boundaries."""
import pathlib
import re
import sys

# These files render attribute paths, inspect editor syntax, or own address
# spelling. None of the other consumers may reconstruct binding membership.
SPELLING_OWNERS = {
    "carina-core/src/resource/binding_address.rs": "decomposition and private address derivation",
    "carina-core/src/resource/loop_origin.rs": "anonymous legacy provenance",
    "carina-core/src/utils.rs": "existing key canonicalizer (carina#4552)",
    "carina-core/src/references.rs": "attribute paths",
    "carina-core/src/explicit.rs": "attribute paths",
    "carina-core/src/state_document.rs": "JSON attribute-index diagnostics",
    "carina-core/src/identifier/mod.rs": "identity feature paths",
    "carina-core/src/differ/comparison.rs": "attribute comparison paths",
    "carina-core/src/resource/snapshot.rs": "secret attribute paths",
    "carina-cli/src/error.rs": "provider account-list diagnostic parsing",
    "carina-tui/src/ui/detail.rs": "attribute display paths",
    "carina-lsp/src/completion/mod.rs": "authored editor syntax",
    "carina-lsp/src/completion/values.rs": "authored editor syntax",
}
SPELLING = re.compile(r"\.(?:contains|find|rfind)\(\s*['\"]\[['\"]\s*\)|format!\s*\(\s*\"[^\"\n]*\{[^}\n]*\}\[\{[^}\n]*\}\]")
KEY_SPELLING = re.compile(r"\bmap_key_address\s*\(")
KEY_OWNERS = {"carina-core/src/resource/binding_address.rs", "carina-core/src/resource/loop_origin.rs", "carina-core/src/utils.rs"}
MINTING = re.compile(r"\bbinding_with_(?:index|map_key)\b|\.with_(?:index|map_key)\(")


def violations(path, source):
    # Inline test modules and standalone test sources contain address examples.
    if "tests" in pathlib.PurePosixPath(path).parts or pathlib.PurePosixPath(path).stem.endswith("tests"):
        return []
    source = re.split(r"\n#\[cfg\(test\)\]\nmod ", source, maxsplit=1)[0]
    findings = []
    for pattern, allowed in [(SPELLING, path in SPELLING_OWNERS), (MINTING, path == "carina-core/src/resource/binding_address.rs"), (KEY_SPELLING, path in KEY_OWNERS)]:
        if not allowed:
            for match in pattern.finditer(source):
                line = source.count("\n", 0, match.start()) + 1
                findings.append(f"{path}:{line}: use decompose_binding_address for membership and BindingLookup for references; only admission may mint child addresses")
    return findings


def main():
    root = pathlib.Path(__file__).resolve().parent.parent
    findings = []
    for source in sorted(root.glob("carina-*/src/**/*.rs")):
        findings.extend(violations(source.relative_to(root).as_posix(), source.read_text()))
    if findings:
        print("\n".join(findings), file=sys.stderr)
        return 1
    print("Binding address boundaries passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
