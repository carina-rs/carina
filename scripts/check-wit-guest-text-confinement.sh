#!/usr/bin/env bash
# Guest decoding stays in one private module with no output/formatting macros.
set -euo pipefail
python3 - <<'PY'
from pathlib import Path
import re, sys
root = Path('carina-plugin-host/src')
violations = []
# Match the other architectural checks: test-only blocks are not production.
def scrub(text, mark_literals=False):
    clean = list(text)
    def blank(start, end):
        for index in range(start, end):
            if clean[index] != '\n': clean[index] = ' '
    raw_pattern = re.compile(r'(?:b?r)(#*)"')
    char_pattern = re.compile(r"'(?:\\.|[^'\\\n])'")
    index = 0
    while index < len(text):
        start = index
        literal = False
        if text.startswith('//', index):
            end = text.find('\n', index)
            index = len(text) if end < 0 else end
        elif text.startswith('/*', index):
            depth, index = 1, index + 2
            while depth and index < len(text):
                if text.startswith('/*', index): depth += 1; index += 2
                elif text.startswith('*/', index): depth -= 1; index += 2
                else: index += 1
        else:
            raw = raw_pattern.match(text, index)
            char = char_pattern.match(text, index)
            if raw:
                literal = True
                terminator = '"' + raw[1]
                index = text.index(terminator, index + len(raw[0])) + len(terminator)
            elif text[index] == '"':
                literal = True
                index += 1
                while index < len(text):
                    if text[index] == '\\': index += 2
                    elif text[index] == '"': index += 1; break
                    else: index += 1
            elif char: literal = True; index += len(char[0])
            else: index += 1; continue
        blank(start, index)
        if literal and mark_literals: clean[start] = 'x'
    return ''.join(clean)
def production(text):
    clean = scrub(text)
    masked = list(clean)
    literals = list(scrub(text, mark_literals=True))
    for cfg in re.finditer(r'#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]', clean):
        start = cfg.start()
        brace = clean.find('{', cfg.end())
        semicolon = clean.find(';', cfg.end())
        if semicolon >= 0 and (brace < 0 or semicolon < brace):
            end = semicolon + 1
        elif brace >= 0:
            depth, end = 1, brace + 1
            while depth and end < len(clean):
                depth += (clean[end] == '{') - (clean[end] == '}')
                end += 1
            if depth:
                raise ValueError('cannot parse test-only block')
        else:
            raise ValueError('cannot parse test-only item')
        for index in range(start, end):
            if masked[index] != '\n': masked[index] = literals[index] = ' '
    return ''.join(masked), ''.join(literals)

# Follow Rust's out-of-line and inline module declarations, including #[path].
# Only the confinement module's name is policy; its files are derived, never
# maintained as a second list that can forget a new decoder/stderr child.
module = re.compile(r'(?P<attrs>(?:#\s*\[[^\]]*\]\s*)*)(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z_0-9]*)\s*(?P<body>[;{])')
def children(path, lo=0, hi=None, directory=None):
    text = path.read_text()
    clean, _ = production(text)
    hi = len(clean) if hi is None else hi
    directory = (path.parent if path.stem in {'lib', 'mod'} else path.parent / path.stem) if directory is None else directory
    cursor = lo
    while match := module.search(clean, cursor, hi):
        name = match['name']
        attrs = text[match.start('attrs'):match.end('attrs')]
        explicit = re.search(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]', attrs)
        if match['body'] == ';':
            if explicit:
                candidates = [path.parent / explicit[1]]
            else:
                candidates = [directory / (name + '.rs'), directory / name / 'mod.rs']
            existing = [p for p in candidates if p.is_file()]
            if len(existing) != 1:
                raise ValueError(f'{path}: ambiguous or missing module {name}: {candidates}')
            yield name, existing[0]
            cursor = match.end()
        else:
            depth, end = 1, match.end()
            while depth and end < hi:
                depth += (clean[end] == '{') - (clean[end] == '}')
                end += 1
            if depth: raise ValueError(f'{path}: cannot parse inline module {name}')
            yield from children(path, match.end(), end - 1, directory / name)
            cursor = end

def subtree(entry):
    owned, pending = set(), [entry]
    while pending:
        path = pending.pop().resolve()
        if path in owned: continue
        owned.add(path)
        pending.extend(child for _, child in children(path))
    return owned

# Start at lib.rs, so aliases or file moves cannot quietly change coverage.
factory = dict(children(root / 'lib.rs'))['wasm_factory']
entry = dict(children(factory))['guest']
owned = subtree(entry)

# Only host-authored deterministic schema names may cross as ordinary strings.
# Every exception must also carry its reason at the function declaration.
allow_returns = {
    ('wasm_factory/guest/guest_store.rs', 'handoff_credential'): 'Outbound credential wire handoff; only opaque SecretPayload inputs reach this adapter.',
    ('wasm_factory/guest/wasm_convert.rs', 'core_saved_attrs_to_wit'): 'Outbound structured saved attributes; strings are wire keys, never diagnostic text.',
    ('wasm_factory/guest/wasm_convert.rs', 'wit_to_core_value_map'): 'Decoded structured attribute map; keys remain schema-addressed data inside confinement.',
    ('wasm_factory/guest/wasm_convert.rs', 'wit_to_core_persisted_value_map'): 'Decoded structured persisted attributes; keys remain data inside confinement.',
    ('wasm_factory/guest/wasm_convert.rs', 'resource_id_wire_key'): 'Outbound host resource address used solely as a guest wire map key.',
    ('wasm_factory/guest/wasm_convert.rs', 'checked_attributes_to_wit'): 'Outbound checked attributes; strings are wire keys, never diagnostic text.',
    ('wasm_factory/guest/wasm_convert.rs', 'json_to_provider_info'): 'Structured provider name from pre-input metadata; display name and version use protected text.',
    ('wasm_factory/guest.rs', 'info_json'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_string_export'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_identity_attributes'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_schemas'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_provider_config_attribute_types'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_provider_config_completions'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_normalize_state'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'call_hydrate_read_state'): 'Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.',
    ('wasm_factory/guest.rs', 'operation'): 'Returns a host-authored static operation label.',
}
seen_allow_returns = set()
for path in sorted(root.parent.rglob('*.rs')):
    path = path.resolve()
    text = path.read_text()
    if path in owned:
        clean, literals = production(text)
    else:
        clean, literals = scrub(text), scrub(text, mark_literals=True)
    if path in owned:
        banned = r'\b(?:eprintln|println|print|eprint|format|write|writeln|dbg|include)\s*!|\b(?:tracing|log)\s*::\s*\w+\s*!'
        banned += r'|\b(?:std\s*::\s*)?io\s*::\s*(?:stderr|stdout)\b|\bwrite_str\b'
        banned += r'|\buse\s+std\s*::\s*io\s*::\s*\{[^}]*\b(?:stderr|stdout)\b'
        banned += r'|\buse\s+(?:(?:tracing|log)\s*::)|#\s*\[\s*(?:tracing|log)\s*::'
        for match in re.finditer(r'\bpanic\s*!\s*[({\[]\s*[^\s)}\]]', literals):
            violations.append(f'{path}:{text.count(chr(10), 0, match.start()) + 1}: panic with arguments in guest confinement')
        for match in re.finditer(banned, clean):
            violations.append(f'{path}:{text.count(chr(10), 0, match.start()) + 1}: forbidden output/formatting in guest confinement')
        exported = r'\bpub(?:\s*\([^)]*\))?\s+(?:(?:async|unsafe|const)\s+)*fn\s+(?P<name>\w+)\b(?P<header>[^;{]*)[;{]'
        for match in re.finditer(exported, clean):
            signature = match['header']
            if '->' not in signature: continue
            returns = signature.split('->', 1)[1]
            if not re.search(r'\bString\b|&\s*(?:\x27\w+\s*)?(?:mut\s+)?str\b|\bVec\s*<\s*u8\s*>', returns): continue
            key = (str(path.relative_to(root.resolve())), match['name'])
            reason = allow_returns.get(key)
            prefix = text[max(0, text.rfind('\n', 0, match.start()) - 500):match.start()]
            if reason and ('confinement-allow-return: ' + reason) in prefix:
                seen_allow_returns.add(key)
            else:
                violations.append(f'{path}:{text.count(chr(10), 0, match.start()) + 1}: exported guest function returns raw text/bytes: {match["name"]}')
    else:
        generated = r'\b(?:version_checked_bindings|wit_types|WitDesiredResource|WitPlanOp|CarinaProvider|CarinaProviderWithHttp|CarinaProviderAws)\b|\bwasmtime\s*::\s*component\s*::\s*bindgen\s*!|\b(?:basic|http|aws)\s*::\s*(?:carina|exports)\s*::'
        for match in re.finditer(generated, clean):
            violations.append(f'{path}:{text.count(chr(10), 0, match.start()) + 1}: generated binding referenced outside guest confinement')
for key in allow_returns.keys() - seen_allow_returns:
    violations.append(f'stale or undocumented raw-return allowlist: {key}')
if violations:
    print('\n'.join(violations))
    sys.exit(1)
print('OK: WIT guest text confinement checks passed.')
PY
