#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/pre-commit"

assert_equals() {
    local expected="$1"
    local actual="$2"
    local message="$3"

    if [ "$expected" != "$actual" ]; then
        echo "Assertion failed: $message"
        echo "Expected: $expected"
        echo "Actual:   $actual"
        exit 1
    fi
}

assert_array_equals() {
    local expected="$1"
    shift

    local actual="$*"
    if [ "$expected" != "$actual" ]; then
        echo "Assertion failed: unexpected scope arguments"
        echo "Expected: $expected"
        echo "Actual:   $actual"
        exit 1
    fi
}

assert_scope() {
    local staged_files="$1"
    local expected_mode="$2"
    local expected_fmt="$3"
    local expected_clippy="$4"

    set_check_scopes "$staged_files"

    assert_equals "$expected_mode" "$check_mode" "unexpected check mode for staged files: $staged_files"
    assert_array_equals "$expected_fmt" "${fmt_scope[@]-}"
    assert_array_equals "$expected_clippy" "${clippy_scope[@]-}"
}

assert_scope $'README.md\nAGENTS.md\nsite/package.json' "skip" "" ""
assert_scope "Cargo.toml" "workspace" "--all" "--all-targets --all-features"
assert_scope "scripts/pre-commit" "workspace" "--all" "--all-targets --all-features"
assert_scope "carina-cli/src/main.rs" "crate" "-p carina-cli" "-p carina-cli --all-targets --all-features"
assert_scope "carina-core/src/foo.rs" "crate" "-p carina-core" "-p carina-core --all-targets --all-features"
assert_scope $'carina-cli/src/main.rs\ncarina-state/src/lib.rs' "crate" "-p carina-cli -p carina-state" "-p carina-cli -p carina-state --all-targets --all-features"
assert_scope $'README.md\nCargo.lock' "workspace" "--all" "--all-targets --all-features"

echo "pre-commit scope tests passed"

assert_equals 10 "${#hook_static_checks[@]}" 'the hook must run the ten approved static checks'
case " ${excluded_static_checks[*]} " in
    *' check-pinned-toolchain '*) ;;
    *) echo 'Assertion failed: check-pinned-toolchain requires Python 3.11+ and must stay excluded'; exit 1 ;;
esac

for script_path in "$SCRIPT_DIR"/check-*.sh; do
    script="${script_path##*/}"
    script="${script%.sh}"
    count=0
    for entry in "${hook_static_checks[@]}" "${excluded_static_checks[@]}"; do
        if [ "$entry" = "$script" ]; then
            count=$((count + 1))
        fi
    done
    assert_equals "1" "$count" "$script must appear in exactly one static-check array"
done

for script in "${hook_static_checks[@]}" "${excluded_static_checks[@]}"; do
    if [ ! -f "$SCRIPT_DIR/$script.sh" ]; then
        echo "Assertion failed: static-check array entry does not exist: $script"
        exit 1
    fi
done

echo "pre-commit static-check classification tests passed"

assert_output_line() {
    local expected="$1"
    if ! printf '%s\n' "$output" | grep -Fxq "$expected"; then
        echo "Assertion failed: missing output line: $expected"
        echo "$output"
        exit 1
    fi
}

mock_staged_git() {
    if [ "$1" = symbolic-ref ]; then echo test-branch; return; fi
    if [ "$1" != diff ]; then return 99; fi
    local argument
    for argument in "$@"; do
        case "$argument" in
            --quiet) return 1 ;;
            --diff-filter=*)
                case "${argument#--diff-filter=}" in
                    *"$staged_status"*) ;;
                    *) return 0 ;;
                esac
                ;;
        esac
    done
    printf '%s\n' "$staged_paths"
}

# Static failures must survive the Rust skip, including deletion-only commits.
staged_paths=README.md
for staged_status in M D; do
    for mock_static_exit in 0 7; do
        hook_exit=0
        output=$(
            git() { mock_staged_git "$@"; }
            run_static_checks() { echo "static checks ran"; return "$mock_static_exit"; }
            cargo() { echo "Unexpected cargo invocation"; return 99; }
            main
        ) || hook_exit=$?
        assert_equals "$mock_static_exit" "$hook_exit" "commits without Rust changes must preserve the static-check status"
        assert_output_line "static checks ran"
    done
done

scratch_dir=$(mktemp -d "${TMPDIR:-/tmp}/carina-pre-commit.XXXXXX")
trap 'rm -rf "$scratch_dir"' EXIT
wait_for_marker() {
    local marker="$1"
    local attempts=0
    while [ ! -f "$marker" ]; do
        if [ "$attempts" -ge 100 ]; then return 1; fi
        sleep 0.05
        attempts=$((attempts + 1))
    done
}

# Exercise success and each failure for modified and deleted Rust paths.
# Shell functions replace Cargo, so no Rust tools run.
for staged_status in M D; do
    if [ "$staged_status" = D ]; then
        staged_paths=carina-core/src/foo.rs
        expected_crate=carina-core
    else
        staged_paths=carina-cli/src/main.rs
        expected_crate=carina-cli
    fi
    for failing_check in none static fmt clippy; do
        concurrency_dir="$scratch_dir/$staged_status-$failing_check"
        mkdir "$concurrency_dir"
        hook_exit=0
        output=$(
            git() { mock_staged_git "$@"; }
            run_static_checks() {
                touch "$concurrency_dir/static-started"
                if ! wait_for_marker "$concurrency_dir/fmt-started" || \
                    ! wait_for_marker "$concurrency_dir/clippy-started"; then
                    echo "Static checks did not overlap both Cargo jobs"
                    return 99
                fi
                echo "static checks ran"
                if [ "$failing_check" = static ]; then return 1; fi
            }
            cargo() {
                local cargo_check="$1"
                shift
                if [ "$cargo_check" = fmt ]; then
                    assert_array_equals "-p $expected_crate -- --check" "$@"
                elif [ "$cargo_check" = clippy ]; then
                    assert_array_equals "-p $expected_crate --all-targets --all-features --no-deps -- -D warnings" "$@"
                else
                    return 99
                fi
                touch "$concurrency_dir/$cargo_check-started"
                if ! wait_for_marker "$concurrency_dir/static-started"; then
                    echo "Cargo $cargo_check did not overlap static checks"
                    return 99
                fi
                echo "mock cargo $cargo_check ran"
                if [ "$failing_check" = "$cargo_check" ]; then return 1; fi
            }
            SKIP_CLIPPY=0
            main
        ) || hook_exit=$?

        expected_exit=1
        if [ "$failing_check" = none ]; then expected_exit=0; fi
        assert_equals "$expected_exit" "$hook_exit" "unexpected hook status for $staged_status $staged_paths when $failing_check fails"

        # Results stay in this order regardless of which job fails first.
        if [ "$failing_check" = fmt ]; then fmt_result=FAILED; else fmt_result=passed; fi
        if [ "$failing_check" = clippy ]; then clippy_result=FAILED; else clippy_result=passed; fi
        reports=$(printf '%s\n' "$output" | sed -E 's/ \([0-9]+s\):?$//' | \
            grep -E '^(static checks ran|Formatting check |Clippy check )')
        assert_equals "$(printf 'static checks ran\nFormatting check %s\nClippy check %s' "$fmt_result" "$clippy_result")" \
            "$reports" "results must report static checks, fmt, then clippy"
    done
done

echo "pre-commit hook result tests passed"

cat >"$scratch_dir/check-pass.sh" <<'SH'
echo "passing stub output"
SH
cp "$scratch_dir/check-pass.sh" "$scratch_dir/check-also-pass.sh"
cat >"$scratch_dir/check-fail.sh" <<'SH'
printf 'failing stub output' >&2
exit 7
SH

hook_static_checks=(check-fail check-pass)
static_exit=0
output=$(run_static_checks "$scratch_dir") || static_exit=$?
assert_equals "1" "$static_exit" "a failing static check must fail the runner"
assert_output_line "failing stub output"
assert_output_line "this static check also runs in CI; fix it now instead of spending a CI round"
if ! printf '%s\n' "$output" | grep -Eq '^check-pass passed \([0-9]+s\)$'; then
    echo "Assertion failed: the passing static check was not reported after a failure"
    echo "$output"
    exit 1
fi

hook_static_checks=(check-pass check-also-pass)
static_exit=0
output=$(run_static_checks "$scratch_dir") || static_exit=$?
assert_equals "0" "$static_exit" "passing static checks must succeed"

echo "pre-commit static-check runner tests passed"
