#!/usr/bin/env bash
# The check definitions shared by CI and local pre-PR verification.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

usage() {
    cat <<'USAGE'
Usage: scripts/verify.sh [--local] [--base <ref>] [KIND...]

No KIND runs all checks, with static checks first. --local only changes
crate scope, using touched-crates.sh (including uncommitted changes).
--base <ref> is passed to touched-crates.sh; it requires --local.

Kinds:
  fmt provider-boundaries error-source-display no-wall-clock-test-assertions
  docs-spellings resources-access cli-hints no-home-env-reads
  crates-not-publishable wit-bindgen-workspace plan-fixtures rust-cache
  vendored-patches check feature-isolation clippy validate-crn example-warnings
  wasm-fixtures nextest isolation provider-cause doctests

CI execution mechanics (not additional checks in the default run):
  nextest-compile                     Compile tests for the main cache
  nextest-archive <file>              Build the shared test archive
  nextest-partition <file> <k>/<n>     Run one archive partition
  isolation [<archive>]              Run the same fixed in-process suites

Checks continue after failures; the final summary lists every failed kind.
USAGE
}

local_mode=false
base_args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --local) local_mode=true; shift ;;
        --base)
            if [[ $# -lt 2 || -z "$2" || "$2" == --* ]]; then
                echo 'verify.sh: --base requires a ref' >&2
                exit 2
            fi
            base_args=(--base "$2")
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        --*) echo "verify.sh: unknown option: $1" >&2; exit 2 ;;
        *) break ;;
    esac
done
if ! "$local_mode" && [[ ${#base_args[@]} -ne 0 ]]; then
    echo 'verify.sh: --base requires --local' >&2
    exit 2
fi

scope=(--workspace)
if "$local_mode"; then
    touched=$(bash scripts/touched-crates.sh "${base_args[@]}")
    scope=()
    if [[ -n "$touched" ]]; then
        read -r -a scope <<< "$touched"
    fi
fi
# Test compilation, archive creation and local execution must select exactly
# the same tests. Archive consumers inherit this selection from the archive.
test_features=(--all-features)
test_selection=("${scope[@]}" "${test_features[@]}")

scoped_cargo() {
    if [[ ${#scope[@]} -eq 0 ]]; then
        echo 'skipped: no touched crates'
        return
    fi
    cargo "$@"
}

run_kind() {
    case "$1" in
        fmt) cargo fmt --all -- --check ;;
        provider-boundaries)
            bash scripts/check-provider-boundaries.sh
            bash scripts/check-wit-guest-text-confinement.sh
            ;;
        error-source-display)
            bash scripts/check-error-source-not-in-display.sh
            bash scripts/test-check-error-source-not-in-display.sh
            ;;
        no-wall-clock-test-assertions)
            bash scripts/test-check-no-wall-clock-test-assertions.sh
            bash scripts/check-no-wall-clock-test-assertions.sh
            ;;
        docs-spellings)
            bash scripts/test-check-docs-spellings.sh
            bash scripts/check-docs-use-new-spellings.sh
            ;;
        resources-access) bash scripts/check-no-direct-resources-access.sh ;;
        cli-hints) bash scripts/check-cli-hints-use-project-command.sh ;;
        no-home-env-reads) bash scripts/check-no-home-env-reads.sh ;;
        crates-not-publishable) bash scripts/check-crates-not-publishable.sh ;;
        wit-bindgen-workspace)
            bash scripts/check-wit-bindgen-workspace.sh
            bash scripts/check-provider-sdk-bindings.sh
            bash scripts/test-check-provider-sdk-bindings.sh
            bash scripts/check-test-utils-dev-only.sh
            bash scripts/test-check-test-utils-dev-only.sh
            ;;
        plan-fixtures) bash scripts/check-plan-fixtures.sh ;;
        rust-cache)
            bash scripts/check-rust-cache-via-local-action.sh
            bash scripts/test-classify-ci-diff.sh
            bash scripts/test-rust-cache-key.sh
            bash scripts/test-verify-sh.sh
            ;;
        vendored-patches) bash scripts/check-vendored-patches-used.sh ;;
        check) scoped_cargo check "${scope[@]}" --all-targets --all-features ;;
        feature-isolation) scoped_cargo hack check "${scope[@]}" --each-feature --no-dev-deps ;;
        clippy) scoped_cargo clippy "${scope[@]}" --all-targets --all-features -- -D warnings ;;
        validate-crn) bash scripts/validate-crn-files.sh ;;
        example-warnings) bash scripts/check-example-warnings.sh ;;
        wasm-fixtures)
            cargo build -p carina-provider-mock -p carina-provider-mock-http --target wasm32-wasip2
            ;;
        nextest) scoped_cargo nextest run "${test_selection[@]}" --no-fail-fast ;;
        nextest-compile) scoped_cargo nextest run "${test_selection[@]}" --no-run ;;
        nextest-archive)
            scoped_cargo nextest archive "${test_selection[@]}" --archive-file "$2" --zstd-level 1
            ;;
        nextest-partition)
            cargo nextest run --archive-file "$2" \
                --workspace-remap . --extract-to . --extract-overwrite \
                --partition "count:$3" --no-fail-fast
            ;;
        isolation)
            # Fixed suites, inherently workspace-wide in either mode. An
            # archive already carries the shared test selection above.
            if [[ $# -gt 1 ]]; then
                bash scripts/test-in-process-isolation.sh "$2"
            else
                bash scripts/test-in-process-isolation.sh --build --workspace "${test_features[@]}"
            fi
            ;;
        provider-cause)
            cargo fetch --locked
            bash scripts/check-provider-cause-test-api.sh
            ;;
        doctests) scoped_cargo test "${scope[@]}" --doc --all-features ;;
    esac
}

default_kinds=(
    fmt provider-boundaries error-source-display no-wall-clock-test-assertions
    docs-spellings resources-access cli-hints no-home-env-reads
    crates-not-publishable wit-bindgen-workspace plan-fixtures rust-cache
    vendored-patches check feature-isolation clippy validate-crn example-warnings
    wasm-fixtures nextest isolation provider-cause doctests
)
mechanic_kinds=(nextest-compile nextest-archive nextest-partition)
is_kind() {
    local candidate
    for candidate in "${default_kinds[@]}" "${mechanic_kinds[@]}"; do
        if [[ "$1" == "$candidate" ]]; then return 0; fi
    done
    return 1
}
has_archive_argument() {
    [[ $# -gt 0 ]] && ! is_kind "$1"
}
if [[ $# -eq 0 ]]; then set -- "${default_kinds[@]}"; fi

# Validate the whole invocation before starting any check. Only the execution
# mechanics consume arguments; all other kinds may be combined freely.
requested=("$@")
while [[ $# -gt 0 ]]; do
    kind=$1
    shift
    if ! is_kind "$kind"; then
        echo "verify.sh: unknown kind: $kind" >&2
        usage >&2
        exit 2
    fi
    case "$kind" in
        nextest-archive|nextest-partition)
            if ! has_archive_argument "$@" || [[ -z "$1" || "$1" == --* ]]; then
                echo "verify.sh: $kind requires an archive file" >&2
                exit 2
            fi
            shift
            if [[ "$kind" == nextest-partition ]]; then
                if [[ $# -eq 0 || ! "$1" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]]; then
                    echo 'verify.sh: nextest-partition requires <k>/<n>' >&2
                    exit 2
                fi
                shift
            fi
            ;;
        isolation)
            if has_archive_argument "$@"; then shift; fi
            ;;
    esac
done

set -- "${requested[@]}"
total_start=$SECONDS
failed=()
while [[ $# -gt 0 ]]; do
    kind=$1
    args=("$kind")
    shift
    case "$kind" in
        nextest-archive) args+=("$1"); shift ;;
        nextest-partition) args+=("$1" "$2"); shift 2 ;;
        isolation)
            if has_archive_argument "$@"; then args+=("$1"); shift; fi
            ;;
    esac
    echo "=== $kind ==="
    start=$SECONDS
    # Do not invoke the function in an if/|| condition: Bash would disable
    # errexit inside it and hide failures in multi-command kinds.
    set +e
    (set -e; run_kind "${args[@]}")
    status=$?
    set -e
    echo "=== $kind: exit $status, $((SECONDS - start))s ==="
    if [[ $status -ne 0 ]]; then failed+=("$kind (exit $status)"); fi
done
echo "=== total: $((SECONDS - total_start))s ==="
if [[ ${#failed[@]} -gt 0 ]]; then
    printf 'Failed kinds:\n'
    printf '  %s\n' "${failed[@]}"
    exit 1
fi
echo 'All verify kinds passed.'
