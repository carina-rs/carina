#!/usr/bin/env bash
# Reject rest patterns and functional updates that hide additions to Parts views.
# An optional repository root supports isolated, tracked Rust fixtures.
set -euo pipefail

repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
python3 - "$repo_root" <<'PY'
from pathlib import Path
import os
import re
import subprocess
import sys

ROOT = Path(sys.argv[1]).resolve()
PARTS = {"StructFieldParts", "AttributeSchemaParts", "ResourceSchemaParts", "InputRuleParts"}
PARTS_NAME = re.compile(r"\b(?:" + "|".join(sorted(PARTS)) + r")\b")
TOKENS = re.compile(r"\w+|::|->|\.\.=|\.\.|[^\s]")
RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')
CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^'\\\n])'")
TEST_CFG = re.compile(r"(?m)^[ \t]*#\[cfg\(test\)\]")
ATTRIBUTE = re.compile(r"#\s*\[")
WHITESPACE = re.compile(r"\s*")
MODULE = re.compile(r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+\w+\s*(?P<body>[{;])")
MESSAGE = (
    "Destructure *Parts views exhaustively: name every field and do not use `..`. "
    "These views exist so that adding a schema field breaks every conversion until the field "
    "is consciously carried across (or bound and documented as unavailable on the destination side); "
    "`..` makes a new field silently disappear at the protocol/WASM seam (carina#3772). "
    "Build schema structs through their builders and read them through `parts()`."
)


def blank(clean, start, end):
    clean[start:end] = ["\n" if char == "\n" else " " for char in clean[start:end]]


def scrub(source):
    # Keep offsets and newlines while masking comments (including nested blocks),
    # strings/raw strings, and chars. Lifetimes remain code. This is a lexical
    # guard: it tracks aliases per file but does not expand macros or evaluate cfgs.
    clean = list(source)
    index = 0
    while index < len(source):
        start = index
        if source.startswith("//", index):
            end = source.find("\n", index)
            index = len(source) if end < 0 else end
        elif source.startswith("/*", index):
            depth, index = 1, index + 2
            while depth and index < len(source):
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
        else:
            raw = RAW_STRING.match(source, index)
            char = CHAR.match(source, index)
            if raw:
                terminator = '"' + raw.group(1)
                end = source.find(terminator, raw.end())
                index = len(source) if end < 0 else end + len(terminator)
            elif source[index] == '"':
                index += 1
                while index < len(source):
                    if source[index] == "\\":
                        index += 2
                    elif source[index] == '"':
                        index += 1
                        break
                    else:
                        index += 1
            elif char:
                index = char.end()
            else:
                index += 1
                continue
        blank(clean, start, min(index, len(source)))
    return "".join(clean)


def delimiter_end(code, start, opening, closing):
    depth, end = 1, start + 1
    while depth and end < len(code):
        depth += (code[end] == opening) - (code[end] == closing)
        end += 1
    return end


def production(code):
    # Skip each cfg(test) module only through its matching brace, not to EOF:
    # wasm_convert.rs has production items after reference_tests. Other test-only
    # items (e.g. helpers in schema/mod.rs) must not truncate production coverage.
    # Test-only items outside a cfg(test) module are deliberately still checked.
    clean = list(code)
    for cfg in TEST_CFG.finditer(code):
        cursor = cfg.end()
        while True:
            cursor = WHITESPACE.match(code, cursor).end()
            attribute = ATTRIBUTE.match(code, cursor)
            if not attribute:
                break
            cursor = delimiter_end(code, attribute.end() - 1, "[", "]")
        module = MODULE.match(code, cursor)
        if module:
            end = module.end()
            if module.group("body") == "{":
                end = delimiter_end(code, end - 1, "{", "}")
            blank(clean, cfg.start(), end)
    return "".join(clean)


def skip_generics(tokens, cursor):
    if cursor < len(tokens) and tokens[cursor].group() == "<":
        depth = 1
        cursor += 1
        while depth and cursor < len(tokens):
            value = tokens[cursor].group()
            depth += (value == "<") - (value == ">")
            cursor += 1
    return cursor


def parts_names(code, tokens):
    aliases = []
    for statement in re.finditer(r"\buse\b[^;]*;", code):
        for original, alias in re.findall(r"\b(\w+)\s+as\s+(\w+)\b", statement.group()):
            aliases.append((alias, {original}))
    for index, token in enumerate(tokens):
        if token.group() != "type" or index + 1 >= len(tokens):
            continue
        alias = tokens[index + 1].group()
        cursor = skip_generics(tokens, index + 2)
        if cursor >= len(tokens) or tokens[cursor].group() != "=":
            continue
        targets, stack = set(), []
        cursor += 1
        while cursor < len(tokens):
            value = tokens[cursor].group()
            if value == ";" and not stack:
                break
            if value in ("{", "(", "["):
                stack.append({"{": "}", "(": ")", "[": "]"}[value])
            elif stack and value == stack[-1]:
                stack.pop()
            targets.add(value)
            cursor += 1
        aliases.append((alias, targets))
    names = set(PARTS)
    while True:
        found = {alias for alias, targets in aliases if targets & names}
        if found <= names:
            return names
        names.update(found)


def impl_headers(tokens, names):
    bodies, headers = {}, set()
    for index, token in enumerate(tokens):
        if token.group() != "impl":
            continue
        if index and tokens[index - 1].group() not in ("{", "}", ";", "]", "unsafe", "default"):
            continue  # `impl Trait` in a parameter or return type is not an item.
        cursor = skip_generics(tokens, index + 1)
        self_names, in_where, stack = set(), False, []
        while cursor < len(tokens):
            value = tokens[cursor].group()
            if not stack:
                if value in ("{", ";"):
                    break
                if value == "<":
                    cursor = skip_generics(tokens, cursor)
                    continue
                if value == "where":
                    in_where = True
                if not in_where:
                    if value == "for":
                        self_names.clear()
                    else:
                        self_names.add(value)
            if value in ("(", "["):
                stack.append({"(": ")", "[": "]"}[value])
            elif stack and value == stack[-1]:
                stack.pop()
            cursor += 1
        if cursor < len(tokens) and tokens[cursor].group() == "{":
            bodies[cursor] = bool(self_names & names)
            headers.update(range(index, cursor))
    return bodies, headers


def rest_positions(code):
    tokens = list(TOKENS.finditer(code))
    names = parts_names(code, tokens)
    impl_bodies, headers = impl_headers(tokens, names)
    self_context = []
    for index, token in enumerate(tokens):
        value = token.group()
        if value == "{":
            self_context.append(impl_bodies.get(index, self_context[-1] if self_context else False))
        elif value == "}" and self_context:
            self_context.pop()
        if value not in names and not (value == "Self" and self_context and self_context[-1]):
            continue
        if index in headers:
            continue
        start = index
        while start >= 2 and tokens[start - 1].group() == "::" and tokens[start - 2].group().isidentifier():
            start -= 2
        if start and tokens[start - 1].group() == "::":
            start -= 1
        if start and tokens[start - 1].group() in ("struct", "enum", "->"):
            continue  # Definitions and function bodies are not Parts patterns.
        cursor = index + 1
        if cursor < len(tokens) and tokens[cursor].group() == "::":
            cursor += 1  # Optional turbofish before generics.
        cursor = skip_generics(tokens, cursor)
        if cursor >= len(tokens) or tokens[cursor].group() != "{":
            continue
        stack = ["}"]
        cursor += 1
        while stack and cursor < len(tokens):
            value = tokens[cursor].group()
            if value in ("{", "(", "["):
                stack.append({"{": "}", "(": ")", "[": "]"}[value])
            elif value == stack[-1]:
                stack.pop()
            elif value == ".." and len(stack) == 1 and tokens[cursor - 1].group() in ("{", ","):
                yield tokens[cursor].start()
            cursor += 1


tracked = subprocess.check_output(["git", "ls-files", "-z", "--", "*.rs"], cwd=ROOT)
violations = []
for name in tracked.split(b"\0"):
    if not name:
        continue
    path = Path(os.fsdecode(name))
    if path.parts[0] == "vendor" or "tests" in path.parts:
        continue
    if path.name == "tests.rs" or path.name.endswith("_tests.rs"):
        continue
    try:
        source = (ROOT / path).read_text(encoding="utf-8")
    except FileNotFoundError:
        continue  # Tracked files can be deleted in the working tree.
    # Most tracked Rust files never mention a Parts view. Avoid a word-boundary
    # regex scan (and lexing) over generated provider sources in the hook.
    if "Parts" not in source or not PARTS_NAME.search(source):
        continue
    for position in rest_positions(production(scrub(source))):
        line = source.count("\n", 0, position) + 1
        violations.append("{}:{}: {}".format(path, line, MESSAGE))

if violations:
    print("\n".join(violations), file=sys.stderr)
    sys.exit(1)
print("OK: schema Parts views are destructured exhaustively.")
PY
