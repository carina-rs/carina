#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
checker="$repo_root/scripts/check-schema-parts-exhaustive.sh"
# Keep all fixtures in this worktree's ignored scratch area, even on macOS.
mkdir -p "$repo_root/.carina"
fixtures=$(mktemp -d "$repo_root/.carina/schema-parts-check.XXXXXX")
trap 'rm -rf "$fixtures"' EXIT
git init --quiet --template= "$fixtures"
mkdir -p "$fixtures/src" "$fixtures/tests/nested" "$fixtures/vendor"

cat >"$fixtures/src/multiline.rs" <<'RS'
let StructFieldParts {
    name,
    field_type,
    ..
} = field.parts();
RS
cat >"$fixtures/src/single.rs" <<'RS'
let StructFieldParts { a, b, .. } = value;
RS
cat >"$fixtures/src/update.rs" <<'RS'
let view = StructFieldParts { name: "field", ..base };
RS
cat >"$fixtures/src/qualified.rs" <<'RS'
let schema::StructFieldParts { name, .. } = value;
RS
cat >"$fixtures/src/turbofish.rs" <<'RS'
let view = schema::StructFieldParts::<'_, Draft> { name: "field", ..base };
RS
cat >"$fixtures/src/generic.rs" <<'RS'
let view = StructFieldParts<'_, Wrapper<R>> { name: "field", ..base };
RS
cat >"$fixtures/src/attribute.rs" <<'RS'
let AttributeSchemaParts { name, .. } = value;
RS
cat >"$fixtures/src/resource.rs" <<'RS'
let ResourceSchemaParts { resource_type, .. } = value;
RS
cat >"$fixtures/src/input_rule.rs" <<'RS'
let InputRuleParts { scope, .. } = value;
RS
cat >"$fixtures/src/after_module.rs" <<'RS'
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod tests {
    fn nested() {
        let StructFieldParts { a, b, .. } = value;
        let _ = "}";
    }
}
let StructFieldParts { a, b, .. } = value;
RS
cat >"$fixtures/src/after_helper.rs" <<'RS'
#[cfg(test)]
fn helper() {}
let StructFieldParts { a, b, .. } = value;
RS
cat >"$fixtures/src/exhaustive.rs" <<'RS'
let StructFieldParts { name, field_type } = value;
let StructFieldParts {
    name,
    field_type: Foo { .. },
} = value;
let StructFieldParts { field_type: (head, ..), name } = value;
let StructFieldParts { field_type: [head, ..], name } = value;
let StructFieldParts { name: "..", field_type } = value;
let StructFieldParts { name: r#".. } {"#, field_type } = value;
let StructFieldParts { name: br##".. } {"##, field_type } = value;
let StructFieldParts { name: '}', field_type } = value;
let StructFieldParts { name: '\u{7d}', field_type } = value;
let StructFieldParts { name, /* .. /* nested } */ .. */ field_type } = value;
let StructFieldParts {
    name, // .. }
    field_type,
} = value;
let SimilarStructFieldParts { name, .. } = value;
let StructFieldPartsSuffix { name, .. } = value;
pub struct StructFieldParts<'a> { pub name: &'a str }
pub struct AttributeSchemaParts<'a, R> { pub field_type: &'a R }
pub struct ResourceSchemaParts<'a> { pub resource_type: &'a str }
pub struct InputRuleParts<'a> { pub target: &'a str }
// let StructFieldParts { .. } = value;
/* let ResourceSchemaParts { .. } = value; */
let example = "let StructFieldParts { .. } = value;";
let raw_example = r##"let InputRuleParts { .. } = value;"##;
let escaped = "\" StructFieldParts { .. }";
#[cfg(test)]
#[allow(dead_code)]
mod tests {
    let StructFieldParts { a, b, .. } = value;
}
#[cfg(test)] mod inline_tests { let InputRuleParts { scope, .. } = value; }
#[cfg(test)]
mod external_tests;
RS
for file in src/schema_tests.rs src/tests.rs tests/nested/parts.rs vendor/parts.rs; do
  cat >"$fixtures/$file" <<'RS'
let StructFieldParts { a, b, .. } = value;
RS
done
git -C "$fixtures" add -- '*.rs'
# An untracked production file must not influence the tracked-source scan.
cp "$fixtures/src/single.rs" "$fixtures/src/untracked.rs"

if bash "$checker" "$fixtures" >"$fixtures/output" 2>&1; then
  echo "checker unexpectedly accepted non-exhaustive Parts fixtures" >&2
  cat "$fixtures/output" >&2
  exit 1
fi

message='Destructure *Parts views exhaustively: name every field and do not use `..`. These views exist so that adding a schema field breaks every conversion until the field is consciously carried across (or bound and documented as unavailable on the destination side); `..` makes a new field silently disappear at the protocol/WASM seam (carina#3772). Build schema structs through their builders and read them through `parts()`.'
for location in \
  src/multiline.rs:4 \
  src/single.rs:1 \
  src/update.rs:1 \
  src/qualified.rs:1 \
  src/turbofish.rs:1 \
  src/generic.rs:1 \
  src/attribute.rs:1 \
  src/resource.rs:1 \
  src/input_rule.rs:1 \
  src/after_module.rs:9 \
  src/after_helper.rs:3
do
  if ! grep -Fxq "$location: $message" "$fixtures/output"; then
    echo "checker did not flag $location with the required diagnostic" >&2
    cat "$fixtures/output" >&2
    exit 1
  fi
done
if [ "$(wc -l <"$fixtures/output" | tr -d ' ')" -ne 11 ]; then
  echo "checker reported unexpected fixtures or duplicate diagnostics" >&2
  cat "$fixtures/output" >&2
  exit 1
fi

# A separate passing run checks the exit status as well as the absence of false
# positives; no fixture depends on Rust sources in the real repository.
for file in multiline single update qualified turbofish generic attribute resource input_rule after_module after_helper; do
  git -C "$fixtures" rm --quiet --force -- "src/$file.rs"
done
if ! bash "$checker" "$fixtures" >"$fixtures/output" 2>&1; then
  echo "checker reported an exhaustive, nested, definition, or test-only fixture" >&2
  cat "$fixtures/output" >&2
  exit 1
fi
if ! grep -Fxq 'OK: schema Parts views are destructured exhaustively.' "$fixtures/output"; then
  echo "checker did not report success" >&2
  cat "$fixtures/output" >&2
  exit 1
fi

echo "OK: schema Parts exhaustive checker fixtures passed."
