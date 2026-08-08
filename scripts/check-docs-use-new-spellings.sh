#!/usr/bin/env bash
# Fail if hand-written markdown ships a scanned fenced code block using
# pre-naming-conventions primitive or resource spellings. Explicitly non-DSL
# language fences are exempt; DSL-like, unknown, and untagged fences are scanned.
# With no file arguments, scan tracked markdown and exclude historical artifacts
# under notes/. With file arguments, scan exactly the files provided.
set -euo pipefail

offenders=()

check_file() {
  local f="$1"
  awk '
    in_fence && /^```[[:space:]]*$/ {
      in_fence = 0
      skip_fence = 0
      next
    }
    !in_fence && /^```/ {
      in_fence = 1
      tag = $0
      sub(/^```[[:space:]]*/, "", tag)
      sub(/[[:space:]].*$/, "", tag)

      # Carina DSL may be tagged hcl/text or left untagged, so those and unknown
      # tags remain scanned. Only explicit non-DSL language tags are skipped;
      # nobody tags Carina DSL as rust or json.
      skip_fence = (tag ~ /^(rust|json|yaml|toml|ts|tsx|js|jsx|javascript|typescript|python|go)$/)
      next
    }
    in_fence && !skip_fence {
      bad = 0

      # Match ": <old primitive>" in type-annotation position.
      if ($0 ~ /(^|[^:]):[[:space:]]*(string|int|bool|float|aws_account_id|ipv4_cidr|arn|kms_key_arn|iam_role_arn|iam_policy_arn)([^[:alnum:]_]|$)/) {
        bad = 1
      }

      # Match a lowercase resource immediately after a known service.
      if ($0 ~ /(^|[^[:alnum:]_.\/])(aws|awscc)\.(ec2|s3|iam|sso|logs|sqs)\.[a-z][a-z0-9_]*([^[:alnum:]_]|$)/) {
        bad = 1
      }

      # Match service-less lowercase resource paths. Requiring every segment from
      # the provider through the final segment to be lowercase avoids PascalCase
      # type paths with lowercase enum value tails.
      if ($0 ~ /(^|[^[:alnum:]_.\/-])(aws|awscc)(\.[a-z][a-z0-9_]*)+([^[:alnum:]_.]|$)/) {
        bad = 1
      }

      if (bad) {
        printf "%s:%d: %s\n", FILENAME, FNR, $0
        found = 1
      }
    }
    END { exit found ? 1 : 0 }
  ' "$f" || offenders+=("$f")
}

if [ "$#" -gt 0 ]; then
  for f in "$@"; do
    check_file "$f"
  done
else
  while IFS= read -r f; do
    case "$f" in
      notes/*) continue ;;
    esac
    check_file "$f"
  done < <(git ls-files '*.md')
fi

if [ ${#offenders[@]} -ne 0 ]; then
  echo
  echo "Found old-spelling usage in hand-written docs:"
  printf '  %s\n' "${offenders[@]}"
  exit 1
fi

echo "docs OK"
