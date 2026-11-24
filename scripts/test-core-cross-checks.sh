#!/usr/bin/env bash
# Exercise the runner with recording substitutes, without Rust tools.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 - "$repo_root" <<'PY'
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO = Path(sys.argv.pop(1)).resolve()
FILTER = (
    'not (binary(=validate_diagnostic_order) | binary(=diagnostic_replay) | test(/reference_visit/)'
    ' | (binary(=aws_credentials_preflight_e2e) & test(=apply_without_aws_credentials_fails_fast_with_the_typed_startup_error))'
    ' | (binary(=conversion_round3) & test(=r5c_boundary_conversions_stop_before_operational_export_checks))'
    ' | (binary(=conversion_round3) & test(=round3_read_only_commands_accept_the_same_config_with_a_conversion))'
    ')'
)


class CoreCrossChecksTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="carina-core-cross-checks-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("core-cross-checks.sh", "verify.sh", "wasm-fixture-crates.txt"):
            shutil.copyfile(REPO / "scripts" / name, scripts / name)
        (scripts / "touched-crates.sh").write_text(
            ': > "$CROSS_CHECK_SCOPE_TRACE"\n'
            'if [[ $# -gt 0 ]]; then printf "%s\\0" "$@" > "$CROSS_CHECK_SCOPE_TRACE"; fi\n'
            'printf "%s\\n" "$CROSS_CHECK_SCOPE"\n'
            'exit "${CROSS_CHECK_SCOPE_STATUS:-0}"\n'
        )
        (scripts / "require-wasm-fixtures.sh").write_text(
            'echo \'["require-wasm-fixtures"]\' >> "$CROSS_CHECK_TRACE"\n'
            'exit "${CROSS_CHECK_FIXTURE_STATUS:-0}"\n'
        )
        binary = self.root / "bin/cargo"
        binary.parent.mkdir()
        binary.write_text(
            '#!/usr/bin/env python3\n'
            'import json, os, sys\n'
            'with open(os.environ["CROSS_CHECK_TRACE"], "a") as trace:\n'
            '    trace.write(json.dumps(["cargo", *sys.argv[1:]]) + "\\n")\n'
            'sys.exit(int(os.environ.get("CROSS_CHECK_" + sys.argv[1].upper() + "_STATUS", "0")))\n'
        )
        binary.chmod(0o755)
        rustup = binary.with_name("rustup")
        rustup.write_text('#!/bin/sh\nprintf "%s\\n" wasm32-wasip2\n')
        rustup.chmod(0o755)
        self.trace = self.root / "trace"
        self.scope_trace = self.root / "scope-trace"
        self.env = dict(os.environ, PATH=f"{binary.parent}:{os.environ['PATH']}",
                        CROSS_CHECK_TRACE=str(self.trace), CROSS_CHECK_SCOPE="--workspace",
                        CROSS_CHECK_SCOPE_TRACE=str(self.scope_trace))
        self.env.pop("CARGO_TARGET_DIR", None)
        self.env.pop("CARGO_BUILD_TARGET_DIR", None)
        self.env.pop("CDPATH", None)
        for command in ("SCOPE", "BUILD", "NEXTEST", "TEST", "FIXTURE"):
            self.env[f"CROSS_CHECK_{command}_STATUS"] = "0"
        packages = []
        for crate in (scripts / "wasm-fixture-crates.txt").read_text().splitlines():
            packages.extend(["-p", crate])
        self.expected = [
            ["cargo", "build", *packages, "--target", "wasm32-wasip2"],
            ["require-wasm-fixtures"],
            ["cargo", "nextest", "run", "-p", "carina-core", "-p", "carina-cli", "-p", "carina-lsp",
             "--all-features", "--no-fail-fast", "-E", FILTER],
            ["cargo", "test", "--doc", "-p", "carina-core", "--all-features"],
        ]

    def run_checks(self, *args, **environment):
        self.trace.write_text("")
        result = subprocess.run(
            ["bash", str(self.root / "scripts/core-cross-checks.sh"), *args],
            cwd=self.root / "scripts", env=dict(self.env, **environment),
            capture_output=True, text=True, timeout=30,
        )
        calls = [json.loads(line) for line in self.trace.read_text().splitlines()]
        return result, calls

    def test_non_workspace_scope_skips_cargo(self):
        for scope in ("-p carina-cli", ""):
            with self.subTest(scope=scope):
                result, calls = self.run_checks(CROSS_CHECK_SCOPE=scope)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [])
                self.assertEqual(len(result.stdout.splitlines()), 1)
                self.assertIn("carina-core", result.stdout)
                self.assertIn("nothing was run", result.stdout)

    def test_workspace_runs_fixtures_suites_and_doctests_and_forwards_base(self):
        result, calls = self.run_checks("--base", "refs/heads/main")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, self.expected)
        self.assertEqual(self.scope_trace.read_bytes(), b"--base\0refs/heads/main\0")

    def test_both_suites_run_and_either_failure_fails_the_script(self):
        for nextest, doctest in (("7", "0"), ("0", "9"), ("7", "9")):
            with self.subTest(nextest=nextest, doctest=doctest):
                result, calls = self.run_checks(CROSS_CHECK_NEXTEST_STATUS=nextest,
                                                CROSS_CHECK_TEST_STATUS=doctest)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, self.expected)

    def test_touched_crates_failure_propagates_without_cargo(self):
        result, calls = self.run_checks(CROSS_CHECK_SCOPE_STATUS="23")
        self.assertEqual(result.returncode, 23)
        self.assertEqual(calls, [])

    def test_mismatched_target_fails_before_cargo_with_hint(self):
        configured_target = self.root / "shared-target"
        configured_target.mkdir()
        # Cover a missing target and an existing directory pointing elsewhere.
        for existing in (False, True):
            with self.subTest(existing=existing):
                if existing:
                    (self.root / "target").mkdir()
                result, calls = self.run_checks(CARGO_TARGET_DIR=str(configured_target))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])
                self.assertIn("target/wasm32-wasip2/debug regardless of CARGO_TARGET_DIR", result.stderr)
                self.assertIn("25 tests fail spuriously", result.stderr)
                self.assertIn(f"ln -s -- {configured_target} target", result.stderr)
                self.assertNotIn("$CARGO_TARGET_DIR", result.stderr)
                if existing:
                    self.assertIn("if target is a real directory, move or remove it first", result.stderr)

    def test_target_symlink_to_missing_configured_directory_proceeds_to_fixture_build(self):
        target = self.root / "target"
        for variable in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"):
            with self.subTest(variable=variable):
                configured_target = self.root / variable.lower() / "not-built-yet"
                target.unlink(missing_ok=True)
                target.symlink_to(configured_target.relative_to(self.root))
                result, calls = self.run_checks(**{variable: str(configured_target)})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(configured_target.is_dir())
                self.assertEqual(calls, self.expected)

    def test_cdpath_does_not_shadow_repository_target_or_relative_configured_path(self):
        configured_target = self.root / "shared-target"
        configured_target.mkdir()
        (self.root / "target").symlink_to(configured_target.name)
        decoy = self.root / "decoy"
        (decoy / "target").mkdir(parents=True)
        (decoy / configured_target.name).mkdir()
        for configured_path in (str(configured_target), configured_target.name):
            with self.subTest(configured_path=configured_path):
                result, calls = self.run_checks(CARGO_TARGET_DIR=configured_path, CDPATH=str(decoy))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, self.expected)

    def test_cdpath_cannot_hide_mismatched_repository_target(self):
        configured_target = self.root / "shared-target"
        configured_target.mkdir()
        (self.root / "target").mkdir()
        decoy = self.root / "decoy"
        decoy.mkdir()
        (decoy / "target").symlink_to(configured_target)
        result, calls = self.run_checks(CARGO_TARGET_DIR=str(configured_target), CDPATH=str(decoy))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])

    def test_cargo_build_target_dir_mismatch_fails_before_cargo(self):
        configured_target = self.root / "build-target"
        configured_target.mkdir()
        (self.root / "target").mkdir()
        result, calls = self.run_checks(CARGO_BUILD_TARGET_DIR=str(configured_target))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertIn("target/wasm32-wasip2/debug", result.stderr)
        self.assertIn(f"ln -s -- {configured_target} target", result.stderr)

    def test_cargo_target_dir_takes_precedence_over_cargo_build_target_dir(self):
        cargo_target = self.root / "cargo-target"
        cargo_target.mkdir()
        build_target = self.root / "build-target"
        build_target.mkdir()
        target = self.root / "target"
        for configured_target in (cargo_target, build_target):
            with self.subTest(configured_target=configured_target):
                target.unlink(missing_ok=True)
                target.symlink_to(configured_target.name)
                result, calls = self.run_checks(CARGO_TARGET_DIR=str(cargo_target),
                                                CARGO_BUILD_TARGET_DIR=str(build_target))
                if configured_target == cargo_target:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(calls, self.expected)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(calls, [])

    def test_target_symlinks_are_compared_by_resolved_directory(self):
        (self.root / "shared-target").mkdir()
        (self.root / "target").symlink_to("shared-target")
        (self.root / "target-alias").symlink_to("shared-target")
        result, calls = self.run_checks(CARGO_TARGET_DIR="target-alias", CDPATH=str(self.root))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, self.expected)

    def test_fixture_build_or_guard_failure_stops_before_suites(self):
        for variable, expected in (("CROSS_CHECK_BUILD_STATUS", self.expected[:1]),
                                   ("CROSS_CHECK_FIXTURE_STATUS", self.expected[:2])):
            with self.subTest(variable=variable):
                result, calls = self.run_checks(**{variable: "7"})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, expected)


unittest.main()
PY
