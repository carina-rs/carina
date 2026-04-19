#!/usr/bin/env bash
# Enforce: application code must iterate ParsedFile resources via
# ParsedFile::iter_all_resources() so per-attribute checkers stay in sync
# with resources hidden inside deferred for-expression bodies.
#
# Direct access to parsed.resources is allowed only when explicitly marked
# with `// allow: direct — <reason>` on the same line, where <reason> is
# one of the phrasings below.
#
# See docs/specs/2026-04-19-unify-resource-walk-design.md for rationale.

set -euo pipefail

ALLOWED_REASONS=(
  "parser-internal, pre-expansion"
  "topology (dependency sort)"
  "module expansion, handled separately"
  "plan-time reconciliation"
  "display/reporting"
  "fixture test inspection"
)

scan_dirs=(
  carina-core/src
  carina-cli/src
  carina-lsp/src
)

# Pattern: `parsed.resources.` as a field access chain. This targets the
# `ParsedFile::resources` field specifically, not every struct with a
# `.resources` field (state file, module signatures, etc. have their own
# conventions).
matches=$(grep -rn --include='*.rs' -E 'parsed\.resources\.' "${scan_dirs[@]}" || true)

bad=()
while IFS= read -r line; do
  [ -z "$line" ] && continue

  # Skip the new API itself.
  if echo "$line" | grep -q 'iter_all_resources'; then
    continue
  fi

  # Skip lines with an allow marker and a known reason.
  if echo "$line" | grep -q '// allow: direct'; then
    ok=0
    for reason in "${ALLOWED_REASONS[@]}"; do
      if echo "$line" | grep -qF "// allow: direct — $reason"; then
        ok=1
        break
      fi
    done
    if [ $ok -eq 1 ]; then
      continue
    fi
    echo "Error: allow marker with unrecognized reason: $line" >&2
    bad+=("$line")
    continue
  fi

  # Pure-comment line mentioning parsed.resources (e.g., docs): skip.
  if echo "$line" | grep -qE ':\s*///?\s'; then
    continue
  fi

  bad+=("$line")
done <<< "$matches"

if [ ${#bad[@]} -gt 0 ]; then
  echo "Direct access to parsed.resources without // allow: direct marker:" >&2
  for b in "${bad[@]}"; do
    echo "  $b" >&2
  done
  echo "" >&2
  echo "Prefer ParsedFile::iter_all_resources() so per-attribute checkers see" >&2
  echo "resources inside deferred for-expression bodies." >&2
  exit 1
fi
