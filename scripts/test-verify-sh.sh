#!/usr/bin/env bash
# Guard the single CI/local check definition. Optional root supports fixtures:
#   bash scripts/test-verify-sh.sh /path/to/temporary/repo-copy
set -euo pipefail
repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
verify="$repo_root/scripts/verify.sh"
required_kinds=$(bash "$verify" --list-kinds=ci)
# BSD awk cannot accept literal newlines in a -v assignment.
required_kinds=${required_kinds//$'\n'/ }
if [[ ! -f "$repo_root/.github/workflows/ci.yml" ]]; then
    echo 'ERROR: ci.yml is missing; cannot check verify.sh kind coverage' >&2
    exit 1
fi

# Parse run scalars by indentation, including inline, literal and folded
# forms. This deliberately needs only awk/Bash, not a YAML package.
shopt -s globstar nullglob
for workflow in "$repo_root"/.github/workflows/**/* "$repo_root"/.github/actions/**/action.yml; do
    [[ -f "$workflow" ]] || continue
    awk -v source="${workflow#"$repo_root"/}" -v required_kinds="$required_kinds" '
function allowed_script(script) {
    if (script == "scripts/verify.sh") return 1 # The shared dispatcher.
    # Diff classification selects jobs; it does not perform verification.
    if (source == ".github/workflows/ci.yml" && job == "changes" &&
        script == "scripts/classify-ci-diff.sh") return 1
    # Archive consumers must require their downloaded WASM artifacts.
    if (source == ".github/workflows/ci.yml" &&
        (job == "test-partition" || job == "test-isolation") &&
        script == "scripts/require-wasm-fixtures.sh") return 1
    # Cache-key generation is setup for the local cache action.
    if (source == ".github/actions/rust-cache/action.yml" &&
        script == "scripts/rust-cache-key.sh") return 1
    return 0
}
function allowed_cargo(line) {
    # These commands produce distributable binaries, rather than run checks.
    if (source == ".github/workflows/ci.yml" && job == "binary-build" &&
        line ~ /^[[:space:]]*cargo build --release --bin carina[[:space:]]*$/) return 1
    # The release workflow installs its cross builder and builds artifacts.
    if (source == ".github/workflows/release.yml" && job == "build" &&
        line ~ /^[[:space:]]*cargo (install cross --git https:\/\/github.com\/cross-rs\/cross|build --release --target .+ --bin carina --bin carina-lsp)[[:space:]]*$/) return 1
    return 0
}
function violation(i, line) {
    printf "ERROR: %s:%d (%s) bypasses verify.sh: %s\n", source, lines[i], job, line > "/dev/stderr"
    failed = 1
}
function record_ci_kinds(run,    rest, words, n, i, kind, archive) {
    if (source != ".github/workflows/ci.yml") return
    gsub(/\\\n[[:space:]]*/, " ", run)
    while (match(run, /(^|[;&|\n])[[:space:]]*(bash[[:space:]]+)?["\047]?(\.\/)?scripts\/verify\.sh["\047]?([[:space:]]|$)/)) {
        rest = substr(run, RSTART + RLENGTH)
        run = rest
        sub(/\n.*/, "", rest)
        sub(/^[[:space:]]+/, "", rest)
        n = split(rest, words, /[[:space:]]+/)
        for (i = 1; i <= n; i++) {
            kind = words[i]
            if (!(kind in required)) break
            seen[kind] = 1
            # CI runs the default nextest check in archive partitions.
            if (kind == "nextest-partition") seen["nextest"] = 1
            if (kind == "isolation") {
                archive = words[i + 1]
                if (archive != "" && archive !~ /^[-;&|#]/ &&
                    !(archive in required))
                    seen["isolation-with-archive"] = 1
            }
        }
    }
}
function inspect(    i, line, rest, script, run) {
    run = ""
    for (i = 1; i <= count; i++) {
        line = body[i]
        sub(/^[[:space:]]*#[^\n]*/, "", line)
        run = run (run_folded ? " " : "\n") line
        if (line ~ /(^|[^[:alnum:]_-])(cargo(-[[:alnum:]_-]+)?|\$CARGO|\$\{CARGO\})([[:space:]"\047]|$)/ && !allowed_cargo(line))
            violation(i, line)
        rest = line
        # Require a filename, so the changes filter pattern scripts/.* is data.
        while (match(rest, /scripts\/[[:alnum:]_.\/-]*[[:alnum:]_-][[:alnum:]_.\/-]*/)) {
            script = substr(rest, RSTART, RLENGTH)
            if (!allowed_script(script)) violation(i, line)
            rest = substr(rest, RSTART + RLENGTH)
        }
    }
    record_ci_kinds(run)
    count = 0
}
BEGIN {
    job = "composite"
    n = split(required_kinds, kinds, /[[:space:]]+/)
    for (i = 1; i <= n; i++) required[kinds[i]] = 1
}
/^  [[:alnum:]_-]+:/ {
    inspect()
    if (source ~ /^\.github\/workflows\//) {
        job = $1
        sub(/:$/, "", job)
    }
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
        run_folded = (line ~ /^>/)
        if (line ~ /^[|>]/) next
    } else if (in_run) {
        line = $0
    } else next
    body[++count] = line
    lines[count] = NR
}
END {
    inspect()
    if (source == ".github/workflows/ci.yml") {
        for (kind in required) {
            if (!seen[kind]) {
                printf "ERROR: verify.sh kind %s is not run by ci.yml\n", kind > "/dev/stderr"
                failed = 1
            }
        }
    }
    exit failed
}
' "$workflow"
done

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts" "$fixture/bin"
cp "$verify" "$fixture/scripts/verify.sh"
cp "$repo_root/scripts/wasm-fixture-crates.txt" "$fixture/scripts/wasm-fixture-crates.txt"
# Observe the default dispatch with substitutes: no Rust tools or invariant
# scripts execute. A case arm left out of the default kinds must fail too.
for script in "$repo_root"/scripts/*.sh; do
    name=${script##*/}
    [[ "$name" != verify.sh ]] || continue
    cat > "$fixture/scripts/$name" <<'SH'
printf '%s\n' "${BASH_SOURCE[0]##*/}" >> "$VERIFY_GUARD_TRACE"
SH
done
for tool in cargo cargo-nextest cargo-hack; do
    printf '#!/bin/sh\nexit 0\n' > "$fixture/bin/$tool"
    chmod +x "$fixture/bin/$tool"
done
printf '#!/bin/sh\nprintf "wasm32-wasip2\\n"\n' > "$fixture/bin/rustup"
chmod +x "$fixture/bin/rustup"
touch "$fixture/trace"
if ! PATH="$fixture/bin:$PATH" VERIFY_GUARD_TRACE="$fixture/trace" \
    bash "$fixture/scripts/verify.sh" > "$fixture/output" 2>&1; then
    cat "$fixture/output" >&2
    echo 'ERROR: verify.sh default dispatch failed with recording substitutes' >&2
    exit 1
fi
missing=0
for script in "$repo_root"/scripts/check-*.sh "$repo_root"/scripts/test-*.sh; do
    [[ -e "$script" ]] || continue
    name=${script##*/}
    if ! grep -Fxq "$name" "$fixture/trace"; then
        echo "ERROR: scripts/$name is not run by verify.sh" >&2
        missing=1
    fi
done
if [[ $missing -ne 0 ]]; then exit 1; fi
echo 'OK: workflow/action checks route through verify.sh, CI covers every kind, and every check/test script is included.'
