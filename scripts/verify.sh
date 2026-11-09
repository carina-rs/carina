#!/usr/bin/env bash
# The check definitions shared by CI and local pre-PR verification.
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4) )); then
    echo "verify.sh: Bash 4.4 or newer is required (running $BASH_VERSION)" >&2
    exit 2
fi
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

default_kinds=(
    fmt provider-boundaries error-source-display no-wall-clock-test-assertions
    docs-spellings resources-access cli-hints no-home-env-reads
    crates-not-publishable wit-bindgen-workspace plan-fixtures rust-cache
    vendored-patches check feature-isolation clippy validate-crn example-warnings
    wasm-fixtures nextest isolation provider-cause doctests
)
mechanic_kinds=(nextest-compile nextest-archive nextest-partition)

usage() {
    cat <<'USAGE'
Usage: scripts/verify.sh [--local] [--base <ref>] [KIND...]

No KIND runs all checks, with static checks first. --local only changes
crate scope, using touched-crates.sh (including reverse dependents and
uncommitted changes).
--base <ref> is passed to touched-crates.sh; it requires --local.
--list-kinds prints the default kinds, one per line, without running checks.
--list-kinds=ci also lists CI mechanics (isolation-with-archive means isolation <archive>).

Kinds:
  fmt provider-boundaries error-source-display no-wall-clock-test-assertions
  docs-spellings resources-access cli-hints no-home-env-reads
  crates-not-publishable wit-bindgen-workspace plan-fixtures rust-cache
  vendored-patches check feature-isolation clippy validate-crn example-warnings
  wasm-fixtures nextest isolation provider-cause doctests

CI execution mechanics (each must be the sole kind):
  nextest-compile                     Compile tests for the main cache
  nextest-archive <file>              Build the shared test archive
  nextest-partition <file> <k>/<n>     Run one archive partition
  isolation <archive>                Run the fixed in-process suites from an archive

Checks continue after failures; the final summary lists every failed kind.
USAGE
}

local_mode=false
base_args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --local) local_mode=true; shift ;;
        --list-kinds) printf '%s\n' "${default_kinds[@]}"; exit 0 ;;
        --list-kinds=ci)
            printf '%s\n' "${default_kinds[@]}" "${mechanic_kinds[@]}" isolation-with-archive
            exit 0
            ;;
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

# Test compilation, archive creation and local execution must select exactly
# the same tests. Archive consumers inherit this selection from the archive.
test_features=(--all-features)

is_scoped_kind() {
    case "$1" in
        check|feature-isolation|clippy|wasm-fixtures|nextest|nextest-compile|nextest-archive|isolation|doctests) return 0 ;;
        *) return 1 ;;
    esac
}

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "verify.sh: $kind requires $1 on PATH" >&2
        exit 1
    fi
}

prepare() {
    scope=(--workspace)
    if "$local_mode"; then
        touched=$(bash scripts/touched-crates.sh "${base_args[@]}")
        scope=()
        if [[ -n "$touched" ]]; then read -r -a scope <<< "$touched"; fi
    fi
    test_selection=("${scope[@]}" "${test_features[@]}")
    # Check prerequisites for all selected kinds before executing any check.
    local kind
    for kind in "$@"; do
        if is_scoped_kind "$kind" && [[ ${#scope[@]} -eq 0 ]]; then continue; fi
        case "$kind" in
            fmt|crates-not-publishable|wit-bindgen-workspace|vendored-patches|check|feature-isolation|clippy|validate-crn|example-warnings|wasm-fixtures|nextest|nextest-*|isolation|provider-cause|doctests)
                require_tool cargo ;;
        esac
        case "$kind" in
            nextest|nextest-*|isolation) require_tool cargo-nextest ;;
            feature-isolation) require_tool cargo-hack ;;
            wasm-fixtures|provider-cause)
                require_tool rustup
                if ! rustup target list --installed | grep -Fxq wasm32-wasip2; then
                    echo "verify.sh: $kind requires the wasm32-wasip2 target (rustup target add wasm32-wasip2)" >&2
                    exit 1
                fi
                ;;
        esac
    done
}

run_kind() {
    if is_scoped_kind "$1" && [[ ${#scope[@]} -eq 0 ]]; then
        echo 'skipped: no touched crates'
        return
    fi
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
            bash scripts/test-touched-crates.sh
            ;;
        vendored-patches) bash scripts/check-vendored-patches-used.sh ;;
        check) cargo check "${scope[@]}" --all-targets --all-features ;;
        feature-isolation) cargo hack check "${scope[@]}" --each-feature --no-dev-deps ;;
        clippy) cargo clippy "${scope[@]}" --all-targets --all-features -- -D warnings ;;
        validate-crn) bash scripts/validate-crn-files.sh ;;
        example-warnings) bash scripts/check-example-warnings.sh ;;
        wasm-fixtures)
            local fixture
            local -a fixture_crates fixture_packages=()
            mapfile -t fixture_crates < scripts/wasm-fixture-crates.txt
            for fixture in "${fixture_crates[@]}"; do fixture_packages+=(-p "$fixture"); done
            cargo build "${fixture_packages[@]}" --target wasm32-wasip2
            ;;
        nextest) cargo nextest run "${test_selection[@]}" --no-fail-fast ;;
        nextest-compile) cargo nextest run "${test_selection[@]}" --no-run ;;
        nextest-archive)
            cargo nextest archive "${test_selection[@]}" --archive-file "$2" --zstd-level 1
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
        doctests) cargo test "${scope[@]}" --doc --all-features ;;
    esac
}

is_kind() {
    local candidate
    for candidate in "${default_kinds[@]}" "${mechanic_kinds[@]}"; do
        if [[ "$1" == "$candidate" ]]; then return 0; fi
    done
    return 1
}
usage_error() {
    echo "verify.sh: $*" >&2
    exit 2
}
is_archive() {
    [[ -n "$1" && "$1" != -* ]] && ! is_kind "$1"
}
if [[ $# -eq 0 ]]; then set -- "${default_kinds[@]}"; fi

# Mechanics have exact arguments and run alone; the ordinary loop consumes
# only kind names. A bare isolation remains an ordinary, combinable kind.
expected=0
case "$1" in
    nextest-compile) expected=1 ;;
    nextest-archive) expected=2 ;;
    nextest-partition) expected=3 ;;
    isolation)
        if [[ $# -gt 1 ]] && ! is_kind "$2"; then expected=2; fi ;;
esac
if [[ $expected -gt 0 ]]; then
    if [[ $# -ne $expected ]]; then
        usage_error "$1 must be the sole kind with exactly $((expected - 1)) argument(s)"
    fi
    if [[ $expected -ge 2 ]] && ! is_archive "$2"; then
        usage_error "$1 requires an archive file"
    fi
    if [[ $expected -eq 3 && ! "$3" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]]; then
        usage_error 'nextest-partition requires <k>/<n>'
    fi
    prepare "$1"
    run_kind "$@"
    exit
fi
for kind in "$@"; do
    if ! is_kind "$kind" || [[ "$kind" == nextest-* ]]; then
        usage_error "unknown kind or combined CI mechanic: $kind"
    fi
done
prepare "$@"

total_start=$SECONDS
failed=()
for kind in "$@"; do
    echo "=== $kind ==="
    start=$SECONDS
    # Do not invoke the function in an if/|| condition: Bash would disable
    # errexit inside it and hide failures in multi-command kinds.
    set +e
    (set -e; run_kind "$kind")
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
