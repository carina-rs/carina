#!/usr/bin/env bash
# Keep every field explicit at the typed core/protocol conversion seam.
# An optional repository root supports isolated, cargo-free fixtures.
set -euo pipefail

repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
python3 - "$repo_root" <<'PY'
from pathlib import Path
import re
import sys

repo = Path(sys.argv[1]).resolve()
seam = repo / "carina-core/src/protocol_convert"
if not seam.is_dir():
    sys.exit("ERROR: missing protocol conversion directory: {}".format(seam))

# This is a lexical guard, not a Rust parser: no macro expansion or type
# resolution, and only literal #[cfg(test)] with standard module paths is
# recognized. A bare `..` in a free-function call is indistinguishable from a
# tuple-struct pattern; bind a full range first or use explicit bounds there.
# Functional updates are recognized in braces following a constructor name;
# unusually shaped block expressions may also need a named range binding.
identifier = re.compile(r"[A-Za-z_][A-Za-z_0-9]*\Z")
raw_string = re.compile(r'(?:br|cr|r)(#*)"')
character = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|[^\n])|[^'\\\n])'")


def mask_noncode(source):
    masked = list(source)
    i = 0
    while i < len(source):
        start = i
        literal = False
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
        elif source.startswith("/*", i):
            i += 2
            depth = 1
            while i < len(source) and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
        else:
            raw = raw_string.match(source, i)
            char = character.match(source, i)
            if raw:
                terminator = '"' + raw.group(1)
                end = source.find(terminator, raw.end())
                i = len(source) if end < 0 else end + len(terminator)
                literal = True
            elif source[i] == '"':
                i += 1
                while i < len(source):
                    if source[i] == "\\":
                        i += 2
                    elif source[i] == '"':
                        i += 1
                        break
                    else:
                        i += 1
                literal = True
            elif char:
                i = char.end()
                literal = True
            else:
                i += 1
                continue
        for j in range(start, min(i, len(source))):
            if source[j] != "\n":
                masked[j] = " "
        if literal:
            # Retain an expression token so ranges adjacent to literals do
            # not become apparent bare rest elements after masking.
            masked[start] = "0"
    return "".join(masked)


def tokenize(source):
    tokens = list(re.finditer(r"\.\.=|\.\.|=>|[A-Za-z_][A-Za-z_0-9]*|[^\s]", mask_noncode(source)))
    words = [token.group() for token in tokens]
    pairs = {}
    parents = []
    stack = []
    closing = {"}": "{", ")": "(", "]": "["}
    for i, word in enumerate(words):
        parents.append(stack[-1] if stack else None)
        if word in ("{", "(", "["):
            stack.append(i)
        elif word in closing and stack and words[stack[-1]] == closing[word]:
            opening = stack.pop()
            pairs[opening] = i
            pairs[i] = opening
    return tokens, words, pairs, parents


def test_modules(path, words, pairs):
    excluded = set()
    files = set()
    i = 0
    while i < len(words):
        if words[i:i + 7] != ["#", "[", "cfg", "(", "test", ")", "]"]:
            i += 1
            continue
        j = i + 7
        # Permit other attributes and visibility between cfg(test) and mod.
        while words[j:j + 2] == ["#", "["] and j + 1 in pairs:
            j = pairs[j + 1] + 1
        if words[j:j + 1] == ["pub"]:
            j += 1
            if words[j:j + 1] == ["("] and j in pairs:
                j = pairs[j] + 1
        if j + 2 < len(words) and words[j] == "mod" and identifier.fullmatch(words[j + 1]):
            if words[j + 2] == "{" and j + 2 in pairs:
                end = pairs[j + 2] + 1
                excluded.update(range(i, end))
                i = end
                continue
            if words[j + 2] == ";":
                directory = path.parent if path.name == "mod.rs" else path.with_suffix("")
                files.add(directory / (words[j + 1] + ".rs"))
        i += 1
    return excluded, files


def constructor_brace(words, pairs, opening):
    if opening == 0:
        return False
    constructor = words[opening - 1]
    if not (identifier.fullmatch(constructor) or constructor == ">"):
        return False
    if constructor in ("else", "loop", "unsafe", "const", "try", "async", "move"):
        return False
    # A prefix range can also be a block's result: `if condition { ..end }`
    # or `fn range() -> Range { ..end }`. Walk the enclosing header, jumping
    # over complete argument lists but stopping at an enclosing delimiter.
    cursor = opening - 1
    while cursor >= 0 and words[cursor] not in (";", "{", "}", "(", "["):
        if words[cursor] in ("fn", "if", "while", "for", "match"):
            return False
        if words[cursor] in (")", "]") and cursor in pairs:
            cursor = pairs[cursor]
        cursor -= 1
    return True


sources = {}
test_files = set()
for path in sorted(seam.glob("*.rs")):
    if path.name == "error.rs":
        continue
    source = path.read_text()
    tokens, words, pairs, parents = tokenize(source)
    excluded, files = test_modules(path, words, pairs)
    test_files.update(files)
    if ["#", "!", "[", "cfg", "(", "test", ")", "]"] == words[:8]:
        test_files.add(path)
    sources[path] = (source, tokens, words, pairs, parents, excluded)

hits = []
for path, (source, tokens, words, pairs, parents, excluded) in sources.items():
    if path in test_files:
        continue
    for i, word in enumerate(words):
        if word != ".." or i in excluded:
            continue
        previous = words[i - 1] if i else None
        following = words[i + 1] if i + 1 < len(words) else None
        opening = parents[i]
        if opening is None:
            continue
        delimiter = words[opening]
        rest = False
        if delimiter == "{" and previous in ("{", ","):
            if following == "}":
                rest = True
            else:
                rest = constructor_brace(words, pairs, opening)
        elif delimiter == "(" and previous in ("(", ",") and following in (")", ","):
            method_call = opening >= 2 and words[opening - 2] == "." and bool(
                identifier.fullmatch(words[opening - 1])
            )
            rest = not method_call
        if rest:
            hits.append("{}:{}".format(path.relative_to(repo), source.count("\n", 0, tokens[i].start()) + 1))

if hits:
    print("\n".join(hits), file=sys.stderr)
    print("Name every variant and every field at the core/protocol seam. A catch-all lets a newly added protocol field or variant be dropped silently (carina#3034, carina#3772). If a field is intentionally not carried, bind it by name (e.g. `field: _omitted_reason`) so the omission is visible and compile-checked.", file=sys.stderr)
    sys.exit(1)

print("OK: protocol conversion seam has 0 rest-pattern/update hits.")
PY
