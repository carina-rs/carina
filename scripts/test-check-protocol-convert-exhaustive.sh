#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
checker=${1:-$repo_root/scripts/check-protocol-convert-exhaustive.sh}
# Keep fixtures under the worktree's ignored state directory, never /tmp.
mkdir -p "$repo_root/.carina"
fixtures=$(mktemp -d "$repo_root/.carina/protocol-convert-exhaustive.XXXXXX")
trap 'rm -rf "$fixtures"' EXIT
seam="$fixtures/carina-core/src/protocol_convert"
mkdir -p "$seam"
message='Name every variant and every field at the core/protocol seam. A catch-all lets a newly added protocol field or variant be dropped silently (carina#3034, carina#3772). If a field is intentionally not carried, bind it by name (e.g. `field: _omitted_reason`) so the omission is visible and compile-checked.'

reject() {
    local label=$1
    shift
    cat >"$seam/mod.rs"
    if bash "$checker" "$fixtures" >"$fixtures/output" 2>&1; then
        echo "checker unexpectedly accepted $label" >&2
        exit 1
    fi
    local expected=""
    local line
    for line in "$@"; do
        expected+="carina-core/src/protocol_convert/mod.rs:$line"$'\n'
    done
    expected+="$message"
    if [[ $(cat "$fixtures/output") != "$expected" ]]; then
        echo "checker reported the wrong locations or message for $label" >&2
        cat "$fixtures/output" >&2
        exit 1
    fi
}

# This violation must be ignored for every case, including failures elsewhere.
cat >"$seam/error.rs" <<'RS'
fn classify(error: Error) { match error { Error::Context { .. } => (), _ => () } }
RS

reject 'struct rest' 1 <<'RS'
fn convert(value: Foo) { let Foo { a, .. } = value; }
RS
reject 'multiline struct rest' 4 <<'RS'
fn convert(value: Foo) {
    let Foo {
        a,
        ..
    } = value;
}
RS
reject 'tuple-struct rest' 1 <<'RS'
fn convert(value: E) { if let E::Variant(..) = value {} }
RS
reject 'tuple rest at end' 1 <<'RS'
fn convert(value: (u8, u8)) { let (a, ..) = value; }
RS
reject 'tuple rest at start' 1 <<'RS'
fn convert(value: (u8, u8)) { let (.., a) = value; }
RS
reject 'whole tuple rest' 1 <<'RS'
fn convert(value: (u8, u8)) { let ( .. ) = value; }
RS
reject 'tuple rest in middle' 1 <<'RS'
fn convert(value: (u8, u8, u8)) { let (a, .., b) = value; }
RS
reject 'functional update' 1 <<'RS'
fn convert(base: Foo) -> Foo { Foo { a: 1, ..base } }
RS
reject 'multiline functional update' 4 <<'RS'
fn convert(base: Foo) -> Foo {
    Foo {
        a: 1,
        ..
            base
    }
}
RS
reject 'functional update inside a condition' 2 <<'RS'
fn convert(base: Foo) {
    if inspect(Foo { a: 1, ..base }) {}
}
RS
reject 'functional update in a guarded match arm' 1 <<'RS'
fn c(k: K, base: Foo) -> Foo { match k { K::A if g() => Foo { a: 1, ..base }, } }
RS
reject 'functional update after a guarded match arm' 1 <<'RS'
fn c(k: K, base: Foo) -> Foo { match k { K::A if g() => base, K::B => Foo { a: 1, ..base }, } }
RS
reject 'test-only helper outside a module' 2 <<'RS'
#[cfg(test)]
fn helper(value: Foo) { let Foo { .. } = value; }
RS
reject 'production code before and after a test module' 1 9 <<'RS'
fn before(value: Foo) { let Foo { .. } = value; }
#[cfg(test)]
mod t {
    fn assertion(value: Foo) {
        let Foo { .. } = value;
        let _text = "}";
    }
}
fn after(value: Foo) { let Foo { .. } = value; }
RS

cat >"$seam/mod.rs" <<'RS'
fn ranges(s: &[u8], a: usize, b: usize, n: usize, start: usize) {
    values.drain(..);
    values.get(..);
    let _ = (0..n, a..=b, start.., ..n, 0..);
    let _ = (&s[..], &s[a..]);
    let _ = Foo { range: ..n };
    let _ = Foo { range: start.. };
    let _ = { ..n };
    let _ = 0.."literal";
    let _ = ("literal"..);
    let _ = if condition { ..n } else { ..n };
    let _ = match condition { true => { ..n }, false => { ..n } };
    let _ = async move { ..n };
}
fn prefix_range() -> std::ops::RangeTo<usize> { ..n }
// Foo { .. }
/* Foo { .. } /* nested Variant(..) */ */
const TEXT: &str = "Foo { .. } and escaped \" Variant(..)";
const RAW: &str = r##"Foo { .. } and " Variant(..)"##;
const BYTES: &[u8] = br#"Variant(..)"#;
fn characters<'a>(s: &'a str) { let _ = ('{', '}', '\''); }
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod t {
    fn assertion(value: Foo) {
        let Foo { .. } = value;
        if let E::Variant(..) = value {}
        let _ = Foo { a: 1, ..base };
    }
}
#[cfg(test)]
mod tests;
#[cfg(test)]
mod custom_assertions;
RS
for name in tests custom_assertions; do
    cat >"$seam/$name.rs" <<'RS'
fn assertion(value: Foo) { let Foo { .. } = value; }
RS
done
if ! bash "$checker" "$fixtures" >"$fixtures/output" 2>&1; then
    echo 'checker rejected ranges, comments, strings, or excluded error/test code' >&2
    cat "$fixtures/output" >&2
    exit 1
fi
if ! grep -Fxq 'OK: protocol conversion seam has 0 rest-pattern/update hits.' "$fixtures/output"; then
    cat "$fixtures/output" >&2
    exit 1
fi

# Ordinary submodule files must be checked, even beside test-only files.
cat >"$seam/conversion.rs" <<'RS'
fn convert(value: Foo) { let Foo { .. } = value; }
RS
if bash "$checker" "$fixtures" >"$fixtures/output" 2>&1; then
    echo 'checker unexpectedly skipped a production submodule file' >&2
    exit 1
fi
if ! grep -Fxq 'carina-core/src/protocol_convert/conversion.rs:1' "$fixtures/output"; then
    cat "$fixtures/output" >&2
    exit 1
fi
rm "$seam/conversion.rs"

# Nested production files, including error.rs, must also be checked.
mkdir -p "$seam/sub"
for name in deep error; do
    cat >"$seam/sub/$name.rs" <<'RS'
fn convert(value: Foo) { let Foo { .. } = value; }
RS
    if bash "$checker" "$fixtures" >"$fixtures/output" 2>&1; then
        echo "checker unexpectedly skipped a nested production file: sub/$name.rs" >&2
        exit 1
    fi
    if [[ $(cat "$fixtures/output") != "carina-core/src/protocol_convert/sub/$name.rs:1"$'\n'"$message" ]]; then
        echo "checker reported the wrong location or message for sub/$name.rs" >&2
        cat "$fixtures/output" >&2
        exit 1
    fi
    rm "$seam/sub/$name.rs"
done

echo 'OK: protocol-convert exhaustiveness checker fixtures passed.'
