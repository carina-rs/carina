#!/usr/bin/env bash
# Guard the single CI/local check definition. Optional root supports fixtures:
#   bash scripts/test-verify-sh.sh /path/to/temporary/repo-copy
set -euo pipefail
repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
workflow="$repo_root/.github/workflows/ci.yml"
verify="$repo_root/scripts/verify.sh"

# Parse run scalars by indentation, including inline, literal and folded
# forms. This deliberately needs only awk/Bash, not a YAML package.
awk '
function inspect(    i, line) {
    for (i = 1; i <= count; i++) {
        line = body[i]
        sub(/^[[:space:]]*#[^\n]*/, "", line)
        if (line !~ /(^|[^[:alnum:]_-])cargo([[:space:]]|$)/ &&
            line !~ /scripts\/(check-[[:alnum:]_-]+|test-[[:alnum:]_-]+)\.sh/)
            continue
        # Explicit setup exceptions: the changes detector and test-result
        # aggregator are orchestration; binary-build is the release artifact
        # producer, not a verification job. All stay unchanged in CI.
        if (job == "changes" || job == "test-result" || job == "binary-build")
            continue
        printf "ERROR: ci.yml:%d (%s) bypasses verify.sh: %s\n", lines[i], job, line > "/dev/stderr"
        failed = 1
    }
    count = 0
}
/^  [[:alnum:]_-]+:/ {
    inspect()
    job = $1
    sub(/:$/, "", job)
    in_run = 0
}
{
    match($0, /[^ ]/)
    indent = RSTART - 1
    if (in_run && $0 !~ /^[[:space:]]*$/ && indent <= run_indent) {
        inspect()
        in_run = 0
    }
    if ($0 ~ /^[[:space:]]*(- )?run:/) {
        inspect()
        run_indent = indent
        if ($0 ~ /^[[:space:]]*- run:/) run_indent += 2
        in_run = 1
        line = $0
        sub(/^[[:space:]]*(- )?run:[[:space:]]*/, "", line)
    } else if (in_run) {
        line = $0
    } else next
    body[++count] = line
    lines[count] = NR
}
END { inspect(); exit failed }
' "$workflow"

# Other explicitly unchanged setup steps (rustc toolchain output,
# require-wasm-fixtures.sh, safe.directory and check-examples echo) do not
# match check/test script or cargo invocations. There are currently no
# deliberately non-CI check-*.sh or test-check-*.sh exceptions.
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts" "$fixture/bin"
cp "$verify" "$fixture/scripts/verify.sh"
# Observe the default dispatch with substitutes: no Rust tools or invariant
# scripts execute. A case arm left out of the default kinds must fail too.
for script in "$repo_root"/scripts/*.sh; do
    name=${script##*/}
    [[ "$name" != verify.sh ]] || continue
    cat > "$fixture/scripts/$name" <<'SH'
printf '%s\n' "${BASH_SOURCE[0]##*/}" >> "$VERIFY_GUARD_TRACE"
SH
done
printf '#!/bin/sh\nexit 0\n' > "$fixture/bin/cargo"
chmod +x "$fixture/bin/cargo"
touch "$fixture/trace"
if ! PATH="$fixture/bin:$PATH" VERIFY_GUARD_TRACE="$fixture/trace" \
    bash "$fixture/scripts/verify.sh" > "$fixture/output" 2>&1; then
    cat "$fixture/output" >&2
    echo 'ERROR: verify.sh default dispatch failed with recording substitutes' >&2
    exit 1
fi
missing=0
for script in "$repo_root"/scripts/check-*.sh "$repo_root"/scripts/test-check-*.sh; do
    [[ -e "$script" ]] || continue
    name=${script##*/}
    if ! grep -Fxq "$name" "$fixture/trace"; then
        echo "ERROR: scripts/$name is not run by verify.sh" >&2
        missing=1
    fi
done
if [[ $missing -ne 0 ]]; then exit 1; fi
echo 'OK: CI checks route through verify.sh and every check script is included.'
