#!/usr/bin/env bash
# check-attribute-type-ref-peel.sh
#
# Repo-invariant: every `match` / `matches!` on `AttributeType` shape
# variants in `carina-core` and `carina-lsp` MUST either
#   (a) include an explicit `AttributeType::Ref(...)` arm, or
#   (b) be preceded (within a small window above) by a
#       `.resolve_refs(...)` / `ResolvedAttrType` call, or
#   (c) opt out with a `# allow-raw-attribute-type-match` line comment.
#
# Without one of those, a wildcard arm silently swallows `Ref` and the
# walker drops cyclic-CFN attributes — the carina#3349 bug class. The
# `ResolvedAttrType` newtype in `carina-core/src/schema/resolved_attr_type.rs`
# enforces this at the type level for callers that *do* call
# `resolve_refs`; this script catches the residual case of new callers
# who forget to call it at all.
#
# See CLAUDE.md "Root-cause fixes only" and carina#3340 / carina#3349.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

scopes=(
    "$ROOT/carina-core/src"
    "$ROOT/carina-lsp/src"
)

# Files where raw `AttributeType` matching is intentional and audited:
# - schema/mod.rs defines the enum and Schema::validate / canonicalize
#   walkers that pattern-match every variant including Ref.
# - schema/resolved_attr_type.rs is the newtype implementation.
# - schema/tests.rs / *tests*.rs construct test fixtures.
# - provider.rs collect_*_from_type walks structurally with explicit Ref arms.
# - utils.rs / value.rs / inference.rs / detail_rows.rs / lint.rs /
#   diagnostics/mod.rs / diagnostics/validation.rs /
#   completion/values.rs / differ/comparison.rs already audited for
#   carina#3349; new sites in these files must still be peel-safe but
#   are spot-checked manually because the file is large.
allowlist=(
    "schema/mod.rs"
    "schema/resolved_attr_type.rs"
    "schema/tests.rs"
    "schema/types.rs"
    "provider.rs"
    "diagnostics/checks.rs"
    "diagnostics/tests/mod.rs"
    "tests.rs"
)

is_allowlisted() {
    local path="$1"
    for entry in "${allowlist[@]}"; do
        if [[ "$path" == *"$entry" ]]; then
            return 0
        fi
    done
    return 1
}

fail=0

for scope in "${scopes[@]}"; do
    while IFS= read -r -d '' file; do
        if is_allowlisted "$file"; then
            continue
        fi
        # Find lines that match on AttributeType shape variants AND have
        # a wildcard arm `_ =>` within the next 40 lines, AND do NOT
        # have a `resolve_refs(` call within the 6 lines above the
        # match, AND do NOT carry the audit-opt-out comment.
        awk '
        function flush(start_line, file) {
            if (in_match && have_wildcard && !has_ref_arm) {
                # Look back at the previous 6 lines for resolve_refs or opt-out
                for (j = (start_line - 6 > 1 ? start_line - 6 : 1); j < start_line; j++) {
                    if (window[j] ~ /resolve_refs|allow-raw-attribute-type-match|ResolvedAttrType/) {
                        in_match = 0; have_wildcard = 0; has_ref_arm = 0; return
                    }
                }
                # Also check the opt-out anywhere inside the match body
                for (j = start_line; j <= NR; j++) {
                    if (window[j] ~ /allow-raw-attribute-type-match/) {
                        in_match = 0; have_wildcard = 0; has_ref_arm = 0; return
                    }
                }
                printf("%s:%d: raw match on AttributeType shape with wildcard arm but no `resolve_refs(...)` peel above and no `AttributeType::Ref(...)` arm — `Ref` may be silently swallowed (carina#3349). Either peel via `resolve_refs(defs).as_attr()`, add an explicit Ref arm, or add a `// allow-raw-attribute-type-match: <reason>` comment.\n",
                       file, start_line)
                bad++
            }
            in_match = 0; have_wildcard = 0; has_ref_arm = 0
        }
        {
            window[NR] = $0
            # Heuristic: a match opens when we see `match` (or `matches!`)
            # mentioning AttributeType:: or attr_type / field_type.
            if ($0 ~ /\b(match|matches!)\b/ && $0 ~ /AttributeType|attr_type|field_type|attr\.attr_type|attr_schema\.attr_type/) {
                flush(match_line, FILENAME)
                in_match = 1
                match_line = NR
                have_wildcard = 0
                has_ref_arm = 0
                brace = 1
            } else if (in_match) {
                if ($0 ~ /AttributeType::Ref/) has_ref_arm = 1
                if ($0 ~ /^[[:space:]]*_[[:space:]]*=>/) have_wildcard = 1
                # bound the look-ahead at 40 lines so we donot dragnet
                if (NR - match_line > 40) flush(match_line, FILENAME)
            }
        }
        END { flush(match_line, FILENAME); exit (bad > 0 ? 1 : 0) }
        ' "$file" || fail=1
    done < <(find "$scope" -name '*.rs' -print0)
done

if [[ $fail -ne 0 ]]; then
    echo "" >&2
    echo "FAIL: at least one raw AttributeType shape-match risks silently dropping Ref." >&2
    echo "See carina#3349 and ResolvedAttrType in carina-core/src/schema/resolved_attr_type.rs." >&2
    exit 1
fi

echo "OK: every match on AttributeType shape variants either peels Ref via resolve_refs, includes an explicit Ref arm, or is allowlisted."
