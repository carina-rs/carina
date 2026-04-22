#!/usr/bin/env bash
# Fail if any hand-written markdown file under this repo ships a ```crn``` block
# using the pre-naming-conventions spellings (snake_case primitives or snake_case
# custom types in type position). Provider reference docs under
# docs/src/content/docs/reference/providers/ are codegen output and are
# excluded here; they move when the provider repo regenerates them.
set -euo pipefail

offenders=()

while IFS= read -r f; do
  case "$f" in
    docs/src/content/docs/reference/providers/*) continue ;;
    docs/handoff/*) continue ;;
  esac

  awk '
    /^```crn[[:space:]]*$/ { in_crn = 1; next }
    /^```[[:space:]]*$/    { in_crn = 0; next }
    in_crn {
      # Match ": <lowercase type>" in type-annotation position.
      if (match($0, /:[[:space:]]*(string|int|bool|float|aws_account_id|ipv4_cidr|arn|kms_key_arn|iam_role_arn|iam_policy_arn)\b/)) {
        printf "%s:%d: %s\n", FILENAME, NR, $0
        found = 1
      }
      # Match `aws.<service>.<lowercase_resource>` or similar, excluding enum paths
      # (which can end in snake_case values like .ap_northeast_1 after a PascalCase
      # TypeName segment — those are not resource-kind references).
      if (match($0, /\b(aws|awscc)\.(ec2|s3|iam|sso|logs|sqs)\.[a-z][a-z_0-9]*/)) {
        printf "%s:%d: %s\n", FILENAME, NR, $0
        found = 1
      }
    }
    END { exit found ? 1 : 0 }
  ' "$f" || offenders+=("$f")
done < <(git ls-files '*.md')

if [ ${#offenders[@]} -ne 0 ]; then
  echo
  echo "Found old-spelling usage in hand-written docs:"
  printf '  %s\n' "${offenders[@]}"
  exit 1
fi

echo "docs OK"
