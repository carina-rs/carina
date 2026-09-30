#!/usr/bin/env bash
# Enforce: project-scoped carina commands in production hints must be rendered
# through carina-core's ProjectCommand so the operated-on project path is kept.
#
# This intentionally uses a simple source scan. It skips the renderer itself,
# standalone test files/directories, braced items marked #[cfg(test)], and
# comment-only `//`/Rust doc lines. All other source lines are checked,
# including lines with trailing comments and lines that continue a multi-line
# string literal. The carina-*/src glob covers every workspace crate and
# automatically includes newly added Carina crates.

set -euo pipefail

output_file=$(mktemp)
trap 'rm -f "$output_file"' EXIT

while IFS= read -r file; do
  case "$file" in
    carina-core/src/hint.rs|*/tests.rs|*/tests/*|*_tests.rs)
      continue
      ;;
  esac

  awk -v file="$file" '
    function brace_delta(line, copy, opens, closes) {
      copy = line
      opens = gsub(/\{/, "{", copy)
      copy = line
      closes = gsub(/\}/, "}", copy)
      return opens - closes
    }

    {
      line = $0
      trimmed = line
      sub(/^[[:space:]]*/, "", trimmed)

      if (in_test_item) {
        test_depth += brace_delta(line)
        if (test_depth <= 0) {
          in_test_item = 0
          test_depth = 0
        }
        next
      }

      if (trimmed ~ /^#\[cfg\(test\)\]/) {
        pending_test_item = 1
        next
      }

      if (pending_test_item) {
        # Keep consuming attributes until the cfg(test) item begins.
        if (trimmed == "" || trimmed ~ /^#\[/) {
          next
        }
        depth = brace_delta(line)
        if (depth > 0) {
          in_test_item = 1
          test_depth = depth
        }
        pending_test_item = 0
        next
      }

      if (trimmed ~ /^\/\//) {
        next
      }

      if (line ~ /carina[[:space:]]+(init|plan|apply|destroy|validate|state|providers|export|force-unlock|lint|fmt|module)([^A-Za-z0-9_-]|$)/ ||
          line ~ /carina[[:space:]]+[{]/) {
        printf "%s:%d:%s\n", file, FNR, line
      }
    }
  ' "$file" >> "$output_file"
done < <(
  find carina-*/src -type f -name '*.rs' | sort
)

if [ -s "$output_file" ]; then
  echo "Project-scoped command hint bypasses ProjectCommand:" >&2
  sed 's/^/  /' "$output_file" >&2
  echo >&2
  echo "Render the command with carina_core::hint::ProjectCommand so its project path is preserved." >&2
  exit 1
fi
