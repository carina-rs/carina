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
    "carina-core/src/protocol_convert/mod.rs": "schema field and rule paths",
    "carina-core/src/schema/mod.rs": "schema field and rule paths",
    "carina-sigstore-tlog/src/lib.rs": "transparency-log JSON paths",
    "carina-state/src/backend.rs": "state-document row diagnostics",
    "carina-cli/src/error.rs": "provider account-list diagnostic parsing",
    "carina-tui/src/ui/detail.rs": "attribute display paths",
    "carina-lsp/src/completion/mod.rs": "authored editor syntax",
    "carina-lsp/src/completion/values.rs": "authored editor syntax",
}
SPELLING = re.compile(
    r"\.(?:contains|find|rfind|split_once)\(\s*['\"]\[['\"]\s*\)"
    r"|\.ends_with\(\s*['\"]\]['\"]\s*\)"
    r'''|\.strip_suffix\(\s*"'\]"\s*\)'''
    r'|(?:format!\s*\(\s*|write!\s*\(\s*[^,\n]+,\s*)"[^"\n]*'
    r"(?:\{[^}\n]*\}|[A-Za-z_][A-Za-z_0-9]*)\[(?:\{[^}\n]*\}|'\{[^}\n]*\}')\]"
)
KEY_SPELLING = re.compile(r"\bmap_key_address\s*\(")
KEY_OWNERS = {"carina-core/src/resource/binding_address.rs", "carina-core/src/resource/loop_origin.rs", "carina-core/src/utils.rs"}
MINTING = re.compile(r"\bbinding_with_(?:index|map_key)\b|\.with_(?:index|map_key)\(")
INSTANCE_SPLITTING = re.compile(r"\b(?:name|binding(?:\.as_str\(\))?)\.split_once\(\s*'\.'\s*\)")
INSTANCE_OWNERS = {
    "carina-core/src/module_resolver/expander.rs": {
        "split_instance_prefix":
            "module instance reconciliation splits the namespace prefix, with an unrestricted identity tail; it does not infer loop membership",
    },
    "carina-core/src/schema/type_identity.rs": {
        "from_dotted": "schema type provider/name decomposition",
    },
}

# These literal bases index vectors in a JSON document, never loop bindings.
# In particular plan.rs's {kind}[{index}] reports a saved-plan projection row.
JSON_PATH_BASES = {
    "carina-cli/src/commands/plan.rs": {
        "sorted_resources", "unresolved_resources", "compositions", "data_sources",
        "data_source_origins", "data_source_values", "deferred_for_expressions",
        "wait_bindings", "provider_configs", "plan.effects",
        "current_states", "{location}.deletes", "plan.abandoned_deposed",
        "Invalid saved plan: {kind}", "Invalid saved plan: data_source_values",
        "Invalid saved plan: plan.effects", "Invalid saved plan: deferred_for_expressions",
        "Invalid saved plan replacement metadata: replace_display",
    },
    "carina-state/src/state/mod.rs": {"resources"},
}


def is_json_path(path, match):
    expression = match[0]
    if not expression.startswith(("format!", "write!")):
        return False
    literal_base = expression.split('"', 1)[1].split("[", 1)[0]
    return literal_base in JSON_PATH_BASES.get(path, set())


def is_instance_owner(path, source, match):
    functions = list(re.finditer(r"\bfn\s+(\w+)\b", source[:match.start()]))
    return bool(functions and functions[-1][1] in INSTANCE_OWNERS.get(path, {}))

# Skip test bodies, not the production code that may follow an inline module.
# Strings and comments can contain braces without changing the module depth.
RUST_TOKENS = re.compile(
    r'r(?P<hashes>#+)".*?"(?P=hashes)|r".*?"|"(?:\\.|[^"\\])*"'
    r"|'(?:\\.|[^'\\\n])'|//[^\n]*|/\*.*?\*/|[{}]", re.DOTALL,
)
TEST_MODULE = re.compile(r"(?m)^\s*#\[cfg\(test\)\]\s*mod\s+\w+\s*\{")


def without_test_modules(source):
    position = 0
    while match := TEST_MODULE.search(source, position):
        depth = 1
        end = len(source)
        for token in RUST_TOKENS.finditer(source, match.end()):
            if token[0] == "{":
                depth += 1
            elif token[0] == "}":
                depth -= 1
                if depth == 0:
                    end = token.end()
                    break
        source = source[:match.start()] + re.sub(r"[^\n]", " ", source[match.start():end]) + source[end:]
        position = end
    return source


def violations(path, source):
    # Inline test modules and standalone test sources contain address examples.
    if "tests" in pathlib.PurePosixPath(path).parts or pathlib.PurePosixPath(path).stem.endswith("tests"):
        return []
    source = without_test_modules(source)
    findings = []
    for pattern, allowed in [(SPELLING, path in SPELLING_OWNERS), (MINTING, path == "carina-core/src/resource/binding_address.rs"), (KEY_SPELLING, path in KEY_OWNERS), (INSTANCE_SPLITTING, False)]:
        if not allowed:
            for match in pattern.finditer(source):
                if pattern is SPELLING and is_json_path(path, match):
                    continue
                if pattern is INSTANCE_SPLITTING and is_instance_owner(path, source, match):
                    continue
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
