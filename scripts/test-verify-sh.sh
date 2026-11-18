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
for workflow in "$repo_root"/.github/workflows/**/* \
    "$repo_root"/.github/actions/**/action.yml "$repo_root"/.github/actions/**/action.yaml; do
    [[ -f "$workflow" ]] || continue
    awk -v source="${workflow#"$repo_root"/}" -v required_kinds="$required_kinds" '
function allowed_script(script) {
    if (script == "scripts/verify.sh") return 1 # The shared dispatcher.
    # Diff classification selects jobs; it does not perform verification.
    if (source == ".github/workflows/ci.yml" && job == "changes" &&
        (script == "scripts/classify-ci-diff.sh" || script == "scripts/touched-crates.sh")) return 1
    # Release tag classification selects the publication channel.
    if (source == ".github/workflows/release.yml" && job == "tag" &&
        script == "scripts/release-tag-channel.sh") return 1
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
function valid_verify_command(run,    command) {
    command = run
    # The partition argument is the only expression in a verify invocation.
    gsub(/\$\{\{ matrix\.partition \}\}/, "partition", command)
    return command ~ /^scripts\/verify\.sh([ \t]+[[:alnum:]_.\/-]+)+$/
}
function allowed_step_if() {
    return source == ".github/workflows/ci.yml" && job == "test" &&
        ((step_command == "scripts/verify.sh nextest-compile" &&
          step_if == "github.event_name == \047push\047") ||
         (step_command == "scripts/verify.sh nextest-archive nextest-archive.tar.zst" &&
          step_if == "github.event_name != \047push\047"))
}
function finish_step() {
    if (step_verify && step_has_if && !allowed_step_if()) {
        printf "ERROR: %s:%d (%s) verify step must not use step-level if: %s\n", source, step_if_line, job, step_if > "/dev/stderr"
        failed = 1
    }
    step_open = step_verify = step_has_if = 0
    step_command = step_if = ""
}
function record_ci_kinds(run,    words, n, i, kind, archive) {
    if (source != ".github/workflows/ci.yml") return
    n = split(run, words, /[[:space:]]+/)
    for (i = 2; i <= n; i++) {
        kind = words[i]
        if (!(kind in required)) break
        seen[kind] = 1
        # CI runs the default nextest check in archive partitions.
        if (kind == "nextest-partition") seen["nextest"] = 1
        if (kind == "isolation") {
            archive = words[i + 1]
            if (archive != "" && archive !~ /^-/ && !(archive in required))
                seen["isolation-with-archive"] = 1
        }
    }
}
function inspect(    i, line, rest, script, run) {
    run = ""
    for (i = 1; i <= count; i++) {
        line = body[i]
        run = run (i == 1 ? "" : "\n") line
        sub(/^[[:space:]]*#[^\n]*/, "", line)
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
    sub(/^[[:space:]]+/, "", run)
    sub(/[[:space:]]+$/, "", run)
    if (index(run, "verify.sh")) {
        step_verify = 1
        if (!valid_verify_command(run)) {
            printf "ERROR: %s:%d (%s) bypasses verify.sh: verify run must be a single scripts/verify.sh command: %s\n", source, lines[1], job, run > "/dev/stderr"
            failed = 1
        } else {
            step_command = run
            record_ci_kinds(run)
        }
    }
    count = 0
}
BEGIN {
    job = "composite"
    n = split(required_kinds, kinds, /[[:space:]]+/)
    for (i = 1; i <= n; i++) required[kinds[i]] = 1
}
source == ".github/workflows/ci.yml" && /continue-on-error/ {
    printf "ERROR: %s:%d must not use continue-on-error; verify failures must fail the job\n", source, NR > "/dev/stderr"
    failed = 1
}
/^  [[:alnum:]_-]+:/ {
    inspect()
    finish_step()
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
    if (!in_run) {
        if ($0 ~ /^[[:space:]]*- [[:alnum:]_-]+:/) {
            finish_step()
            step_open = 1
            step_indent = indent
        }
        if (step_open && ((indent == step_indent && $0 ~ /^[[:space:]]*- if:/) ||
                          (indent == step_indent + 2 && $0 ~ /^[[:space:]]*if:/))) {
            step_has_if = 1
            step_if_line = NR
            step_if = $0
            sub(/^[[:space:]]*(- )?if:[[:space:]]*/, "", step_if)
        }
        if (in_env && $0 !~ /^[[:space:]]*$/ && indent <= env_indent) in_env = 0
        if (source == ".github/workflows/ci.yml" && indent >= 4 &&
            $0 ~ /^[[:space:]]*(- )?env:/) {
            in_env = 1
            env_indent = indent + ($0 ~ /^[[:space:]]*- env:/ ? 2 : 0)
        }
        if (in_env && $0 ~ /(^|[[:space:],{])["\047]?(RUSTFLAGS|RUSTDOCFLAGS|CARGO_[[:alnum:]_]*|NEXTEST_[[:alnum:]_]*)["\047]?[[:space:]]*:/) {
            printf "ERROR: %s:%d (%s) job/step env must not override Rust verification settings: %s\n", source, NR, job, $0 > "/dev/stderr"
            failed = 1
        }
    }
    if ($0 ~ /^[[:space:]]*(- )?run:/) {
        inspect()
        run_indent = indent
        if ($0 ~ /^[[:space:]]*- run:/) run_indent += 2
        in_run = 1
        line = $0
        sub(/^[[:space:]]*(- )?run:[[:space:]]*/, "", line)
        if (line ~ /^[|>]/) next
    } else if (in_run) {
        line = $0
    } else next
    body[++count] = line
    lines[count] = NR
}
END {
    inspect()
    finish_step()
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

# An explicit root is a fixture invocation. Only the outer invocation runs the
# synthetic cases, so each case can exercise this guard without recursion.
if [[ $# -eq 0 ]]; then
    python3 - "$repo_root" <<'PY'
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO = Path(sys.argv.pop(1)).resolve()


class VerifyGuardTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="carina-verify-guard-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for directory in ("scripts", ".github"):
            shutil.copytree(REPO / directory, self.root / directory)
        self.ci = self.root / ".github/workflows/ci.yml"
        self.original_ci = self.ci.read_text()

    def guard(self, expected_error=None):
        result = subprocess.run(
            ["bash", str(REPO / "scripts/test-verify-sh.sh"), str(self.root)],
            capture_output=True, text=True, timeout=30,
        )
        if expected_error is None:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn(expected_error, result.stderr)

    def test_valid_workflows_and_both_action_suffixes_pass(self):
        for suffix in ("yml", "yaml"):
            action = self.root / f".github/actions/nested/valid/action.{suffix}"
            action.parent.mkdir(parents=True, exist_ok=True)
            action.write_text("runs:\n  using: composite\n  steps:\n"
                              "    - shell: bash\n      run: scripts/verify.sh fmt\n")
        self.guard()

    def test_both_action_suffixes_reject_direct_checks(self):
        for suffix in ("yml", "yaml"):
            action = self.root / f".github/actions/nested/bypass/action.{suffix}"
            action.parent.mkdir(parents=True, exist_ok=True)
            for run in ("cargo check --workspace", "bash scripts/check-plan-fixtures.sh"):
                with self.subTest(suffix=suffix, run=run):
                    action.write_text("runs:\n  using: composite\n  steps:\n"
                                      f"    - shell: bash\n      run: {run}\n")
                    self.guard(f".github/actions/nested/bypass/action.{suffix}:")
            action.unlink()

    def test_release_tag_channel_is_only_allowed_in_release_tag_job(self):
        self.ci.write_text(self.original_ci.replace(
            "run: scripts/verify.sh clippy\n",
            'run: bash scripts/release-tag-channel.sh "$GITHUB_REF_NAME"\n', 1))
        self.guard("bypasses verify.sh")
        self.ci.write_text(self.original_ci)
        release = self.root / ".github/workflows/release.yml"
        release.write_text(release.read_text().replace("  tag:\n", "  other:\n", 1))
        self.guard("bypasses verify.sh")

    def test_continue_on_error_is_rejected_at_job_and_step_scope(self):
        for value in ("true", "false", "${{ always() }}"):
            for before, after in (
                ("  check:\n", f"  check:\n    continue-on-error: {value}\n"),
                ("      - run: scripts/verify.sh check\n",
                 f"      - run: scripts/verify.sh check\n        continue-on-error: {value}\n"),
                ("      - run: scripts/verify.sh check\n",
                 f"      - continue-on-error: {value}\n        run: scripts/verify.sh check\n"),
            ):
                with self.subTest(value=value, placement=after):
                    self.assertIn(before, self.original_ci)
                    self.ci.write_text(self.original_ci.replace(before, after, 1))
                    self.guard("must not use continue-on-error; verify failures must fail the job")

    def test_missing_ci_kind_is_rejected(self):
        self.ci.write_text(self.original_ci.replace("      - run: scripts/verify.sh fmt\n", ""))
        self.guard("verify.sh kind fmt is not run by ci.yml")

    def test_verify_runs_reject_shell_operators_wrappers_and_multiple_lines(self):
        for command in (
            "scripts/verify.sh clippy || true",
            "true || scripts/verify.sh clippy",
            "scripts/verify.sh clippy && true",
            "scripts/verify.sh clippy; true",
            "scripts/verify.sh clippy | cat",
            "scripts/verify.sh clippy &",
            "scripts/verify.sh clippy > /dev/null",
            "bash scripts/verify.sh clippy",
            "env scripts/verify.sh clippy",
            "./scripts/verify.sh clippy",
            "'scripts/verify.sh' clippy",
            "scripts/verify.sh clippy # comment",
            '|\n          scripts/verify.sh clippy\n          true',
            '>-\n          scripts/verify.sh\n          clippy',
            'scripts/verify.sh clippy $(true)',
            'scripts/verify.sh clippy `true`',
        ):
            with self.subTest(command=command):
                self.ci.write_text(self.original_ci.replace(
                    "run: scripts/verify.sh clippy\n", f"run: {command}\n", 1))
                self.guard("verify run must be a single scripts/verify.sh command")

    def test_verify_step_conditions_are_rejected_before_and_after_run(self):
        original = "      - run: scripts/verify.sh clippy\n"
        for condition in ("false", "true", "${{ always() }}", "github.event_name != 'push'"):
            for replacement in (
                f"      - if: {condition}\n        run: scripts/verify.sh clippy\n",
                f"      - run: scripts/verify.sh clippy\n        if: {condition}\n",
                f"      - name: Verify\n        if: {condition}\n        run: scripts/verify.sh clippy\n",
            ):
                with self.subTest(condition=condition, replacement=replacement):
                    self.ci.write_text(self.original_ci.replace(original, replacement, 1))
                    self.guard("verify step must not use step-level if")

    def test_only_exact_archive_and_compile_step_conditions_are_allowed(self):
        for kind, condition in (("nextest-compile", "github.event_name == 'push'"),
                                ("nextest-archive nextest-archive.tar.zst", "github.event_name != 'push'")):
            for replacement in ("false", "true", "${{ " + condition + " }}"):
                with self.subTest(kind=kind, replacement=replacement):
                    before = f"if: {condition}\n"
                    # Restrict the mutation to this verify step, retaining
                    # the legitimate conditions on artifact-upload steps.
                    run = f"        run: scripts/verify.sh {kind}\n"
                    prefix, suffix = self.original_ci.split(run, 1)
                    position = prefix.rindex(before)
                    prefix = prefix[:position] + prefix[position:].replace(before, f"if: {replacement}\n", 1)
                    self.ci.write_text(prefix + run + suffix)
                    self.guard("verify step must not use step-level if")

    def test_job_and_step_env_cannot_override_rust_verification_settings(self):
        for name in ("RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_BUILD_JOBS", "CARGO_PROFILE_TEST_DEBUG",
                     "NEXTEST_TEST_THREADS", "NEXTEST_VERSION"):
            for before, replacement in (
                ("  changes:\n", f"  changes:\n    env:\n      {name}: value\n"),
                ("  clippy:\n", f"  clippy:\n    env:\n      {name}: value\n"),
                ("  clippy:\n", f"  clippy:\n    env: {{ {name}: value }}\n"),
                ("      - run: scripts/verify.sh clippy\n",
                 f"      - env:\n          {name}: value\n        run: scripts/verify.sh clippy\n"),
                ("      - run: scripts/verify.sh clippy\n",
                 f"      - run: scripts/verify.sh clippy\n        env:\n          {name}: value\n"),
                ("      - run: scripts/verify.sh clippy\n",
                 f"      - run: scripts/verify.sh clippy\n        env: {{ '{name}': value }}\n"),
                ("      - run: scripts/verify.sh clippy\n",
                 f'      - env: {{ "{name}": value }}\n        run: scripts/verify.sh clippy\n'),
            ):
                with self.subTest(name=name, replacement=replacement):
                    self.ci.write_text(self.original_ci.replace(before, replacement, 1))
                    self.guard("job/step env must not override Rust verification settings")


unittest.main()
PY
fi
