#!/usr/bin/env bash
# Self-test the docs spelling guard with isolated markdown fixtures.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
checker="$script_dir/check-docs-use-new-spellings.sh"
fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/carina-doc-spellings.XXXXXX")"
trap 'rm -rf "$fixture_dir"' EXIT

failures=0
checks=0
checker_output=""
checker_status=0

run_checker() {
  checker_output=""
  checker_status=0
  if checker_output="$(cd "$repo_root" && bash "$checker" "$@" 2>&1)"; then
    checker_status=0
  else
    checker_status=$?
  fi
}

show_checker_output() {
  printf '  checker output:\n'
  while IFS= read -r line; do
    printf '    %s\n' "$line"
  done <<< "$checker_output"
}

record_failure() {
  printf 'FAIL: %s\n' "$1" >&2
  failures=$((failures + 1))
}

assert_rejected() {
  local label="$1"
  local file="$2"
  checks=$((checks + 1))
  run_checker "$file"

  if [ "$checker_status" -eq 0 ]; then
    record_failure "$label: expected exit status non-zero, got 0"
    show_checker_output >&2
    return
  fi

  if [[ "$checker_output" != *"$file"* ]]; then
    record_failure "$label: checker output did not name $file"
    show_checker_output >&2
  fi
}

assert_accepted() {
  local label="$1"
  local file="$2"
  checks=$((checks + 1))
  run_checker "$file"

  if [ "$checker_status" -ne 0 ]; then
    record_failure "$label: expected exit status 0, got $checker_status"
    show_checker_output >&2
  fi
}

assert_repo_accepted() {
  checks=$((checks + 1))
  run_checker

  if [ "$checker_status" -ne 0 ]; then
    record_failure "repository default mode: expected exit status 0, got $checker_status"
    show_checker_output >&2
  fi
}

primitive_type="$fixture_dir/primitive-type.md"
cat > "$primitive_type" <<'EOF'
```crn
enable_https: bool = true
```
EOF

service_resource="$fixture_dir/service-resource.md"
cat > "$service_resource" <<'EOF'
```hcl
instance_tenancy = aws.ec2.vpc
```
EOF

service_less_resource="$fixture_dir/service-less-resource.md"
cat > "$service_less_resource" <<'EOF'
```
let main_vpc = aws.vpc {
}
```
EOF

service_less_type="$fixture_dir/service-less-type.md"
cat > "$service_less_type" <<'EOF'
```hcl
vpc: aws.vpc
```
EOF

readme_excerpt="$fixture_dir/readme-excerpt.md"
cat > "$readme_excerpt" <<'EOF'
```hcl
let web_sg = aws.security_group {
  name   = 'web-sg'
  vpc_id = main_vpc.id
}
aws.security_group.ingress_rule {
  name              = 'http'
  security_group_id = web_sg.id
}
```
EOF

current_spellings="$fixture_dir/current-spellings.md"
cat > "$current_spellings" <<'EOF'
```hcl
let vpc = aws.ec2.Vpc {
  region = aws.Region.ap_northeast_1
  status = aws.s3.BucketVersioning.VersioningStatus.enabled
}
vpc_id: aws.ec2.Vpc
awscc.sso.Assignment {
}
```

```crn
read aws.s3.Bucket {
}
```
EOF

prose_only="$fixture_dir/prose-only.md"
cat > "$prose_only" <<'EOF'
The old aws.vpc spelling was replaced by the current resource name.
EOF

url_examples="$fixture_dir/url-examples.md"
cat > "$url_examples" <<'EOF'
```bash
curl https://aws.amazon.com/ec2/pricing
https://aws.amazon.com
```
EOF

native_rust="$fixture_dir/native-rust.md"
cat > "$native_rust" <<'EOF'
```rust
fn f(x: bool) -> bool {
  x
}
```
EOF

fence_state_reset="$fixture_dir/fence-state-reset.md"
cat > "$fence_state_reset" <<'EOF'
```rust
fn f(x: bool) -> bool {
  x
}
```

```hcl
vpc: aws.vpc
```
EOF

text_old_spelling="$fixture_dir/text-old-spelling.md"
cat > "$text_old_spelling" <<'EOF'
```text
vpc: aws.vpc
```
EOF

assert_rejected "snake_case primitive in type position" "$primitive_type"
assert_rejected "lowercase resource after a known service" "$service_resource"
assert_rejected "service-less resource in an untagged fence" "$service_less_resource"
assert_rejected "service-less resource in type position" "$service_less_type"
assert_rejected "pre-#3707 README excerpt" "$readme_excerpt"
assert_rejected "old spelling in a text fence" "$text_old_spelling"
assert_rejected "old spelling after an exempt fence" "$fence_state_reset"
assert_accepted "current spellings in hcl and crn fences" "$current_spellings"
assert_accepted "old spelling in prose" "$prose_only"
assert_accepted "AWS URLs in a bash fence" "$url_examples"
assert_accepted "native Rust type annotations" "$native_rust"
assert_repo_accepted

if [ "$failures" -ne 0 ]; then
  printf 'Docs spelling checker self-test failed: %d of %d checks failed.\n' \
    "$failures" "$checks" >&2
  exit 1
fi

printf 'Docs spelling checker self-test OK: %d checks passed.\n' "$checks"
