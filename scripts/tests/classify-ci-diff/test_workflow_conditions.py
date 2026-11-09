"""Pin CI gating and main-branch cache writers without running any Rust tools."""

import ast
import json
import os
from pathlib import Path
import re
import shutil
import sys
import subprocess
import tempfile
import textwrap
from types import SimpleNamespace
import unittest

WORKFLOW = Path(sys.argv.pop(1)).resolve()
CODE_JOBS = {
    "check", "feature-isolation", "fmt", "clippy", "check-examples",
    "validate-crn", "check-example-warnings", "test", "binary-build",
}
MAIN_CACHE_JOBS = CODE_JOBS - {"fmt"}
AUX_CACHE_JOBS = {"test-provider-cause", "test-doctests"}
TEST_JOBS = {"test-partition", "test-isolation"} | AUX_CACHE_JOBS
CACHE_JOBS = MAIN_CACHE_JOBS | AUX_CACHE_JOBS
SCRIPT_JOBS = {
    "provider-boundaries", "error-source-display", "no-wall-clock-test-assertions",
    "docs-spellings", "check-resources-access", "check-cli-hints",
    "check-no-home-env-reads", "check-crates-not-publishable",
    "check-wit-bindgen-workspace", "check-plan-fixtures",
    "check-rust-cache-via-local-action", "check-vendored-patches-used",
}
ALL_JOBS = CODE_JOBS | SCRIPT_JOBS | TEST_JOBS | {"changes", "test-result"}


def jobs_from_workflow():
    # Read only the job blocks/one-line conditions used in ci.yml. This is not
    # a general YAML parser: actionlint validates YAML; unexpected condition
    # syntax must fail these tests rather than silently omit a job or gate.
    source = WORKFLOW.read_text().split("\njobs:\n", 1)[1]
    headers = list(re.finditer(r"^  ([a-z][a-z0-9-]*):$", source, re.MULTILINE))
    return {
        header[1]: source[header.end():headers[index + 1].start() if index + 1 < len(headers) else len(source)]
        for index, header in enumerate(headers)
    }


def field(block, name, prefix="    "):
    matches = re.findall(r"^" + prefix + re.escape(name) + r":[ \t]*(.*)$", block, re.MULTILINE)
    if len(matches) > 1:
        raise AssertionError(f"duplicate {name} fields")
    return matches[0] if matches else None


def selected(condition, event, code, version_only, changes_result="success", cache_job=""):
    if condition is None:
        return True
    condition = condition.removeprefix("${{").removesuffix("}}").strip()
    values = {
        "github.event_name": event,
        "inputs.cache-job": cache_job,
        "needs.changes.result": changes_result,
        "needs.changes.outputs.code": str(code).lower(),
        "needs.changes.outputs.version_only": str(version_only).lower(),
    }
    for key, value in values.items():
        condition = condition.replace(key, repr(value))
    condition = condition.replace("always()", "True")
    expression = ast.parse(condition.replace("&&", " and ").replace("||", " or "), mode="eval").body

    def evaluate(node):
        if isinstance(node, ast.Constant) and isinstance(node.value, (str, bool)):
            return node.value
        if isinstance(node, ast.BoolOp):
            return (all if isinstance(node.op, ast.And) else any)(evaluate(value) for value in node.values)
        if isinstance(node, ast.Compare) and len(node.ops) == 1:
            left, right = evaluate(node.left), evaluate(node.comparators[0])
            if isinstance(node.ops[0], ast.Eq):
                return left == right
            if isinstance(node.ops[0], ast.NotEq):
                return left != right
        raise AssertionError(f"unsupported workflow condition: {condition}")

    return evaluate(expression)


class WorkflowConditionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.jobs = jobs_from_workflow()

    def test_gating_matrix(self):
        self.assertEqual(set(self.jobs), ALL_JOBS)
        for label, event, code, version_only, expected in [
            ("docs-only PR", "pull_request", False, False, SCRIPT_JOBS | {"changes"}),
            ("version-only PR", "pull_request", True, True, SCRIPT_JOBS | {"changes"}),
            ("ordinary code PR", "pull_request", True, False, ALL_JOBS - {"binary-build"}),
            ("merge group", "merge_group", True, False, ALL_JOBS - {"binary-build"}),
            ("main push", "push", True, False, MAIN_CACHE_JOBS | {"changes"}),
        ]:
            for name, block in self.jobs.items():
                with self.subTest(case=label, job=name):
                    self.assertEqual(selected(field(block, "if"), event, code, version_only), name in expected)

    def test_every_original_code_gate_is_preserved(self):
        for name in CODE_JOBS:
            self.assertEqual(field(self.jobs[name], "needs"), "changes", name)
            for event in ("pull_request", "merge_group", "push"):
                for version_only in (False, True):
                    with self.subTest(job=name, event=event, version_only=version_only):
                        self.assertFalse(selected(field(self.jobs[name], "if"), event, False, version_only))

    def test_every_rust_cache_job_still_writes_on_main(self):
        cached = {name for name, block in self.jobs.items() if "uses: ./.github/actions/rust-cache" in block}
        self.assertEqual(cached, CACHE_JOBS)
        for name in MAIN_CACHE_JOBS:
            with self.subTest(job=name):
                self.assertTrue(selected(field(self.jobs[name], "if"), "push", True, False))

    def test_version_only_gate_applies_only_to_pull_requests(self):
        for name in CODE_JOBS - {"binary-build"}:
            for event in ("merge_group", "push"):
                with self.subTest(job=name, event=event):
                    self.assertEqual(selected(field(self.jobs[name], "if"), event, True, True), name != "fmt" or event != "push")

    def commands(self, job, event):
        result = []
        for step in re.split(r"(?m)(?=^      - )", self.jobs[job]):
            prefix = r"(?:      - |        )"
            run = field(step, "run", prefix)
            if run is None or not selected(field(step, "if", prefix), event, True, False):
                continue
            if run == ">-":
                run = " ".join(re.findall(r"^          (.*)$", step, re.MULTILINE))
            self.assertNotIn(run, ("|", ">"), "update the test for new multiline run syntax")
            result.append(run)
        return result

    def verify_commands(self, *arguments, scope="--workspace", fail="", missing=(),
                        installed_targets="wasm32-wasip2"):
        """Observe the real dispatcher with recording substitutes, never Cargo."""
        repo = WORKFLOW.parent.parent.parent
        with tempfile.TemporaryDirectory(prefix="carina-verify-") as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            (root / "bin").mkdir()
            shutil.copyfile(repo / "scripts/verify.sh", root / "scripts/verify.sh")
            shutil.copyfile(repo / "scripts/wasm-fixture-crates.txt", root / "scripts/wasm-fixture-crates.txt")
            # Keep real Rust tools out of these tests, including prerequisite
            # discovery. Missing-tool cases must not find the host installation.
            for tool in ("bash", "dirname", "grep"):
                (root / "bin" / tool).symlink_to(shutil.which(tool))
            recorder = root / "bin/cargo"
            if "cargo" not in missing:
                recorder.write_text('#!/usr/bin/env bash\nprintf "cargo %s\\n" "$*" >> "$TRACE"\n'
                                    '[[ "$1" != "$FAIL_COMMAND" ]]\n')
                recorder.chmod(0o755)
            for tool in ("cargo-nextest", "cargo-hack", "rustup"):
                if tool in missing:
                    continue
                substitute = root / "bin" / tool
                substitute.write_text('#!/bin/sh\nprintf "%s\\n" "$INSTALLED_TARGETS"\n'
                                      if tool == "rustup" else '#!/bin/sh\nexit 0\n')
                substitute.chmod(0o755)
            (root / "scripts/touched-crates.sh").write_text(
                'printf "base %s\\n" "$*" >> "$TRACE"\nprintf "%s\\n" "$TOUCHED_SCOPE"\n')
            for script in (repo / "scripts").glob("*.sh"):
                name = script.name
                if name in ("verify.sh", "touched-crates.sh"):
                    continue
                (root / "scripts" / name).write_text(
                    f'printf "{name} %s\\n" "$*" >> "$TRACE"\n')
            trace = root / "trace"
            environment = dict(os.environ, PATH=str(root / "bin"), TRACE=str(trace),
                               TOUCHED_SCOPE=scope, FAIL_COMMAND=fail, INSTALLED_TARGETS=installed_targets)
            result = subprocess.run(["bash", str(root / "scripts/verify.sh"), *arguments],
                                    env=environment, capture_output=True, text=True)
            commands = trace.read_text().splitlines() if trace.exists() else []
            return result, [command.rstrip() for command in commands]

    def test_ci_debug_overrides_are_global_and_shared_by_cache_jobs(self):
        source = WORKFLOW.read_text()
        environment = source.split("\nenv:\n", 1)[1].split("\njobs:\n", 1)[0]
        for profile in ("DEV", "TEST"):
            name = f"CARGO_PROFILE_{profile}_DEBUG"
            self.assertEqual(field(environment, name, "  "), '\"0\"')
            # No job/step may override the shared profile, including main's
            # cache warmer and the restore-only doctest/provider consumers.
            self.assertNotRegex(source, rf"(?m)^ {{4,}}{name}:")
        action = (WORKFLOW.parent.parent / "actions/rust-cache/action.yml").read_text()
        # Swatinem's default environment hash includes all CARGO_* values.
        self.assertNotIn("add-rust-environment-hash-key: false", action)

    def test_push_only_compiles_test_artifacts(self):
        build = "scripts/verify.sh wasm-fixtures"
        archive = "scripts/verify.sh nextest-archive nextest-archive.tar.zst"
        version = "rustc --version | awk '{print \"version=\" $2}' >> \"$GITHUB_OUTPUT\""
        self.assertEqual(self.commands("test", "push"), [version, build, "scripts/verify.sh nextest-compile"])
        for event in ("merge_group", "pull_request"):
            self.assertEqual(self.commands("test", event), [version, build, archive])
        self.assertEqual(self.commands("test-doctests", "pull_request"), ["scripts/verify.sh doctests"])
        for arguments, expected in [
            (("wasm-fixtures", "doctests"), [
                "cargo build -p carina-provider-mock -p carina-provider-mock-http --target wasm32-wasip2",
                "cargo test --workspace --doc --all-features",
            ]),
            (("nextest-compile",), ["cargo nextest run --workspace --all-features --no-run"]),
        ]:
            with self.subTest(arguments=arguments):
                result, commands = self.verify_commands(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(commands, expected)
        self.assertNotIn("shared-key:", self.jobs["test"])
        self.assertNotIn("cache-job:", self.jobs["test"])
        for name in AUX_CACHE_JOBS:
            self.assertIn("cache-job: test", self.jobs[name])
        action = (WORKFLOW.parent.parent / "actions/rust-cache/action.yml").read_text()
        self.assertIn("format('{0}-{1}', steps.manifest.outputs.key, inputs.cache-job)", action)

    def test_provider_cause_fetches_before_offline_checks(self):
        self.assertEqual(self.commands("test-provider-cause", "pull_request"),
                         ["scripts/verify.sh provider-cause"])
        result, commands = self.verify_commands("provider-cause")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["cargo fetch --locked", "check-provider-cause-test-api.sh"])

    def test_archive_consumers_install_the_build_rustc_release(self):
        build = self.jobs["test"]
        self.assertIn("toolchain: ${{ steps.toolchain.outputs.version }}", build)
        self.assertIn("      - id: toolchain", build)
        for job in ("test-partition", "test-isolation"):
            with self.subTest(job=job):
                self.assertNotIn("uses: dtolnay/rust-toolchain@stable", self.jobs[job])
                self.assertIn("uses: dtolnay/rust-toolchain@master", self.jobs[job])
                self.assertIn("toolchain: ${{ needs.test.outputs.toolchain }}", self.jobs[job])

    def test_cache_consumers_never_save_the_build_cache(self):
        action = (WORKFLOW.parent.parent / "actions/rust-cache/action.yml").read_text()
        condition = field(action, "save-if", "        ")
        for event in ("push", "pull_request", "merge_group"):
            for cache_job in ("", "test", "check"):
                with self.subTest(event=event, cache_job=cache_job):
                    self.assertEqual(selected(condition, event, True, False, cache_job=cache_job),
                                     not cache_job and event != "merge_group")

    def test_artifact_uploads_support_failed_job_reruns(self):
        uploads = [step for step in re.split(r"(?m)(?=^      - )", self.jobs["test"])
                   if "uses: actions/upload-artifact@v4" in step]
        self.assertEqual(len(uploads), 2)
        for upload in uploads:
            with self.subTest(upload=upload):
                self.assertIn("overwrite: true", upload)
                self.assertIn("retention-days: 3", upload)

    def test_local_and_archive_isolation_use_the_same_workspace_features(self):
        repo = WORKFLOW.parent.parent.parent
        source = (repo / "scripts/test-in-process-isolation.sh").read_text()
        program = source.split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
        selection = next(node for node in ast.parse(program).body
                         if isinstance(node, ast.If)
                         and any(isinstance(child, ast.Assign)
                                 and any(isinstance(target, ast.Name) and target.id == "build_args"
                                         for target in child.targets)
                                 for child in ast.walk(node)))
        # Inspect only argument selection. Never execute isolation, Cargo or
        # native test binaries from this workflow self-check.
        selection_code = compile(ast.Module(body=[selection], type_ignores=[]), "isolation build_args", "exec")
        for args, expected in [
            (["--build", "--workspace", "--all-features"], ["--workspace", "--all-features"]),
            (["fixtures.tar.zst"], ["--archive-file", "fixtures.tar.zst", "--workspace-remap", ".",
                                    "--extract-to", ".", "--extract-overwrite"]),
        ]:
            namespace = {"sys": SimpleNamespace(argv=["isolation", *args])}
            exec(selection_code, namespace)
            self.assertEqual(namespace["build_args"], expected)
        for args in ([], ["--build"], ["--unknown"], ["first.tar.zst", "second.tar.zst"]):
            with self.subTest(invalid_arguments=args):
                namespace = {"sys": SimpleNamespace(argv=["isolation", *args], stderr=sys.stderr)}
                with self.assertRaises(SystemExit) as error:
                    exec(selection_code, namespace)
                self.assertEqual(error.exception.code, 2)
        result, commands = self.verify_commands("--local", "isolation", scope="-p carina-tui")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["base", "test-in-process-isolation.sh --build --workspace --all-features"])
        result, commands = self.verify_commands("--local", "isolation", scope="")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["base"])
        self.assertEqual(result.stdout.count("skipped: no touched crates"), 1)

        # Exercise the shell entry point as well: bare --build must fail before
        # Cargo discovery, even with the Rust tools unavailable.
        environment = dict(os.environ, CARGO="/nonexistent/carina-cargo")
        result = subprocess.run(["bash", str(repo / "scripts/test-in-process-isolation.sh"), "--build"],
                                env=environment, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("usage:", result.stderr)

    def test_archive_and_fixtures_are_used_by_every_consumer(self):
        self.assertIn("partition: [1, 2, 3, 4, 5, 6]", self.jobs["test-partition"])
        self.assertIn("fail-fast: false", self.jobs["test-partition"])
        for job in ("test-partition", "test-isolation"):
            block = self.jobs[job]
            self.assertEqual(field(block, "needs"), "[changes, test]")
            self.assertIn("name: nextest-archive", block)
            self.assertIn("name: wasm-fixtures", block)
            self.assertIn("path: target/wasm32-wasip2/debug", block)
            self.assertEqual(self.commands(job, "pull_request")[0], "bash scripts/require-wasm-fixtures.sh")
            self.assertNotIn("cargo build", block)
        command = self.commands("test-partition", "pull_request")[1]
        self.assertEqual(command, "scripts/verify.sh nextest-partition nextest-archive.tar.zst ${{ matrix.partition }}/6")
        self.assertEqual(self.commands("test-isolation", "pull_request")[1], "scripts/verify.sh isolation nextest-archive.tar.zst")
        for arguments, expected in [
            (("nextest-archive", "fixtures.tar.zst"),
             "cargo nextest archive --workspace --all-features --archive-file fixtures.tar.zst --zstd-level 1"),
            (("nextest-partition", "fixtures.tar.zst", "2/6"),
             "cargo nextest run --archive-file fixtures.tar.zst --workspace-remap . --extract-to . --extract-overwrite --partition count:2/6 --no-fail-fast"),
            (("isolation", "fixtures.tar.zst"), "test-in-process-isolation.sh fixtures.tar.zst"),
            (("nextest",), "cargo nextest run --workspace --all-features --no-fail-fast"),
        ]:
            with self.subTest(arguments=arguments):
                result, commands = self.verify_commands(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(commands, [expected])

    def test_local_only_changes_crate_scope_and_preserves_failures(self):
        for scope, cargo_scope in [("-p carina-lsp -p carina-cli", "-p carina-lsp -p carina-cli"),
                                   ("--workspace", "--workspace")]:
            result, commands = self.verify_commands("--local", "--base", "main", "check", "feature-isolation",
                                                   "clippy", "nextest", "doctests", scope=scope)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(commands, [
                "base --base main",
                f"cargo check {cargo_scope} --all-targets --all-features",
                f"cargo hack check {cargo_scope} --each-feature --no-dev-deps",
                f"cargo clippy {cargo_scope} --all-targets --all-features -- -D warnings",
                f"cargo nextest run {cargo_scope} --all-features --no-fail-fast",
                f"cargo test {cargo_scope} --doc --all-features",
            ])
        result, commands = self.verify_commands("--local", "check", "feature-isolation", "clippy",
                                               "nextest", "doctests", "isolation", "wasm-fixtures", "fmt", scope="")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["base", "cargo fmt --all -- --check"])
        self.assertEqual(result.stdout.count("skipped: no touched crates"), 7)
        result, commands = self.verify_commands("check", "fmt", "clippy", fail="check")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertEqual(len(commands), 3, commands)
        self.assertIn("check (exit 1)", result.stdout)
        self.assertIn("=== total:", result.stdout)

    def test_default_local_and_ci_runs_have_identical_check_kinds(self):
        ci, ci_commands = self.verify_commands()
        local, local_commands = self.verify_commands("--local", scope="-p carina-cli")
        self.assertEqual(ci.returncode, 0, ci.stdout + ci.stderr)
        self.assertEqual(local.returncode, 0, local.stdout + local.stderr)
        self.assertEqual(local_commands[0], "base")
        self.assertEqual(local_commands[1:], [
            command.replace("--workspace", "-p carina-cli") if command.startswith("cargo ") else command
            for command in ci_commands
        ])
        ci_kinds = re.findall(r"^=== ([a-z-]+) ===$", ci.stdout, re.MULTILINE)
        local_kinds = re.findall(r"^=== ([a-z-]+) ===$", local.stdout, re.MULTILINE)
        self.assertEqual(ci_kinds, local_kinds)
        self.assertIn("nextest", ci_kinds)
        self.assertNotIn("nextest-archive", ci_kinds)

    def test_list_kinds_matches_default_dispatch_without_prerequisites(self):
        default, _ = self.verify_commands()
        result, commands = self.verify_commands(
            "--list-kinds", missing=("cargo", "cargo-nextest", "cargo-hack", "rustup"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, [])
        self.assertEqual(result.stdout.splitlines(),
                         re.findall(r"^=== ([a-z-]+) ===$", default.stdout, re.MULTILINE))
        ci, commands = self.verify_commands(
            "--list-kinds=ci", missing=("cargo", "cargo-nextest", "cargo-hack", "rustup"))
        self.assertEqual(ci.returncode, 0, ci.stderr)
        self.assertEqual(commands, [])
        self.assertEqual(ci.stdout.splitlines(), result.stdout.splitlines() + [
            "nextest-compile", "nextest-archive", "nextest-partition", "isolation-with-archive",
        ])

    def test_ci_mechanics_validate_exact_arguments_before_running_checks(self):
        for arguments in (
            ("nextest-compile", "fmt"), ("fmt", "nextest-compile"),
            ("nextest-archive",), ("nextest-partition",),
            ("nextest-archive", "fixtures.tar.zst", "fmt"),
            ("nextest-partition", "fixtures.tar.zst", "2/6", "fmt"),
            ("fmt", "nextest-archive", "fixtures.tar.zst"),
            ("isolation", "fixtures.tar.zst", "fmt"),
            ("fmt", "isolation", "fixtures.tar.zst"),
            ("nextest-partition", "fixtures.tar.zst"),
            ("nextest-partition", "fixtures.tar.zst", "0/6"),
            ("nextest-partition", "fixtures.tar.zst", "2/0"),
            ("nextest-partition", "fixtures.tar.zst", "wrong"),
        ):
            with self.subTest(arguments=arguments):
                result, commands = self.verify_commands(*arguments)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(commands, [])
                self.assertIn("verify.sh:", result.stderr)
        for kind in ("nextest-archive", "nextest-partition", "isolation"):
            for archive in ("", "-file.tar.zst", "--file.tar.zst", "--build"):
                arguments = [kind, archive] + (["2/6"] if kind == "nextest-partition" else [])
                with self.subTest(arguments=arguments):
                    result, commands = self.verify_commands(*arguments)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertEqual(commands, [])
        # Kind names cannot be archive filenames. With isolation they remain
        # ordinary kind names, so "isolation fmt" is still a valid combination.
        for kind in ("nextest-archive", "nextest-partition"):
            for archive in ("fmt", "isolation", "nextest-compile"):
                arguments = [kind, archive] + (["2/6"] if kind == "nextest-partition" else [])
                with self.subTest(arguments=arguments):
                    result, commands = self.verify_commands(*arguments)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertEqual(commands, [])
        result, commands = self.verify_commands("isolation", "fmt")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["test-in-process-isolation.sh --build --workspace --all-features",
                                    "cargo fmt --all -- --check"])

    def test_missing_prerequisites_fail_before_any_selected_check(self):
        for arguments, missing in [
            (("fmt",), "cargo"),
            (("fmt", "feature-isolation"), "cargo-hack"),
            (("fmt", "nextest"), "cargo-nextest"),
            (("nextest-compile",), "cargo-nextest"),
            (("nextest-archive", "fixtures.tar.zst"), "cargo-nextest"),
            (("nextest-partition", "fixtures.tar.zst", "1/6"), "cargo-nextest"),
            (("isolation", "fixtures.tar.zst"), "cargo-nextest"),
            (("isolation",), "cargo-nextest"),
            (("wasm-fixtures",), "rustup"),
            (("provider-cause",), "rustup"),
        ]:
            with self.subTest(arguments=arguments, missing=missing):
                result, commands = self.verify_commands(*arguments, missing=(missing,))
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(commands, [])
                self.assertIn(f"requires {missing} on PATH", result.stderr)
                self.assertEqual(len(result.stderr.splitlines()), 1)
        for kind in ("wasm-fixtures", "provider-cause"):
            with self.subTest(missing_target=kind):
                result, commands = self.verify_commands("fmt", kind, installed_targets="")
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(commands, [])
                self.assertIn("requires the wasm32-wasip2 target", result.stderr)
                self.assertEqual(len(result.stderr.splitlines()), 1)

    def test_prerequisites_are_only_required_for_checks_that_will_run(self):
        missing = ("cargo-nextest", "cargo-hack", "rustup")
        result, commands = self.verify_commands("fmt", missing=missing)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["cargo fmt --all -- --check"])
        result, commands = self.verify_commands("--local", "check", "feature-isolation", "clippy",
                                               "nextest", "doctests", "isolation", "wasm-fixtures",
                                               scope="", missing=(*missing, "cargo"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, ["base"])
        self.assertEqual(result.stdout.count("skipped: no touched crates"), 7)

    def test_verify_guard_rejects_synthetic_workflow_and_script_drift(self):
        repo = WORKFLOW.parent.parent.parent
        guard = repo / "scripts/test-verify-sh.sh"
        with tempfile.TemporaryDirectory(prefix="carina-verify-drift-") as temporary:
            root = Path(temporary)
            workflow = root / ".github/workflows/ci.yml"
            workflow.parent.mkdir(parents=True)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(repo / "scripts/verify.sh", scripts / "verify.sh")
            shutil.copyfile(repo / "scripts/wasm-fixture-crates.txt", scripts / "wasm-fixture-crates.txt")
            for script in (repo / "scripts").glob("*.sh"):
                if script.name != "verify.sh":
                    (scripts / script.name).touch()

            def audit(source):
                workflow.write_text(source)
                return subprocess.run(["bash", str(guard), str(root)], capture_output=True, text=True)

            original = WORKFLOW.read_text()
            self.assertEqual(audit(original).returncode, 0)
            for style in ("|", ">-"):
                command = "scripts/verify.sh fmt" if style == "|" else "scripts/verify.sh\n          fmt"
                source = original.replace("run: scripts/verify.sh fmt\n",
                                          f"run: {style}\n          {command}\n", 1)
                self.assertEqual(audit(source).returncode, 0)
            for command in (
                "cargo check --all-features",
                "cargo-nextest run --workspace",
                "cargo-hack check --each-feature",
                '\"$CARGO\" check --workspace',
                "${CARGO} check --workspace",
                '\"${CARGO}\" check --workspace',
                "bash scripts/check-plan-fixtures.sh",
                "bash scripts/test-check-docs-spellings.sh",
                "bash scripts/validate-crn-files.sh",
                "python3 scripts/check-synthetic.py",
                "scripts/check-synthetic",
                "bash scripts/nested/check-synthetic.bash",
                "|\n          scripts/verify.sh check\n          env CARGO_BUILD_JOBS=2 cargo check",
                ">-\n          cargo\n          nextest run --workspace",
            ):
                with self.subTest(command=command):
                    result = audit(original.replace("run: scripts/verify.sh check\n", f"run: {command}\n", 1))
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn("bypasses verify.sh", result.stderr)
            workflow.write_text(original)
            # Audit other workflows and composite actions, including scripts
            # whose names do not use the check-/test- convention.
            for path in (root / ".github/workflows/extra.yml",
                         root / ".github/actions/nested/fixture/action.yml"):
                path.parent.mkdir(parents=True, exist_ok=True)
                for command in ("cargo check", "cargo-nextest run", '\"$CARGO\" check',
                                "bash scripts/validate-crn-files.sh", "python3 scripts/check-synthetic.py",
                                "scripts/check-synthetic"):
                    with self.subTest(path=path, command=command):
                        path.write_text("runs:\n  using: composite\n  steps:\n"
                                        f"    - shell: bash\n      run: {command}\n")
                        result = audit(original)
                        self.assertEqual(result.returncode, 1, result.stdout)
                        self.assertIn("bypasses verify.sh", result.stderr)
                        self.assertIn(str(path.relative_to(root)), result.stderr)
                path.unlink()
            # Setup exceptions allow only the setup command, not an arbitrary
            # check added beside it or elsewhere in the same job/action.
            for job, allowed in (("changes", "bash scripts/classify-ci-diff.sh a b"),
                                 ("test-partition", "bash scripts/require-wasm-fixtures.sh"),
                                 ("binary-build", "cargo build --release --bin carina")):
                block = self.jobs[job]
                source = original.replace(block, block + f"      - run: {allowed}\n", 1)
                self.assertEqual(audit(source).returncode, 0)
                result = audit(source.replace(f"run: {allowed}\n",
                                             f"run: {allowed} && bash scripts/validate-crn-files.sh\n", 1))
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("bypasses verify.sh", result.stderr)

            # Every declared default must retain its CI execution, including
            # partitioned nextest. Removing a mechanic must fail separately.
            kinds = subprocess.check_output(["bash", str(scripts / "verify.sh"), "--list-kinds"],
                                            text=True).splitlines()
            for kind in kinds + ["nextest-compile", "nextest-archive", "nextest-partition"]:
                ci_kind = "nextest-partition" if kind == "nextest" else kind
                source, removed = re.subn(
                    r"(?m)^([ \t]*(?:- )?run: )scripts/verify\.sh " + re.escape(ci_kind) + r"(?: [^\n]*)?\n",
                    r"\1echo omitted\n", original)
                self.assertGreater(removed, 0, kind)
                with self.subTest(missing_ci_kind=kind):
                    result = audit(source)
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(f"kind {kind} is not run by ci.yml", result.stderr)
            for mention in ("echo omitted # scripts/verify.sh fmt", "echo 'scripts/verify.sh fmt'",
                            "|\n          scripts/verify.sh check\n          fmt"):
                source = original.replace("run: scripts/verify.sh fmt\n", f"run: {mention}\n", 1)
                with self.subTest(mention=mention):
                    result = audit(source)
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn("kind fmt is not run by ci.yml", result.stderr)
            source = original.replace("scripts/verify.sh isolation nextest-archive.tar.zst",
                                      "scripts/verify.sh isolation")
            result = audit(source)
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("kind isolation-with-archive is not run by ci.yml", result.stderr)

            dispatcher = scripts / "verify.sh"
            original_dispatcher = dispatcher.read_text()
            dispatcher.write_text(original_dispatcher.replace("default_kinds=(\n",
                                                            "default_kinds=(\n    synthetic-kind\n", 1))
            result = audit(original)
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("kind synthetic-kind is not run by ci.yml", result.stderr)
            dispatcher.write_text(original_dispatcher)
            dispatcher.write_text(original_dispatcher.replace("mechanic_kinds=(",
                                                            "mechanic_kinds=(synthetic-mechanic ", 1))
            result = audit(original)
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("kind synthetic-mechanic is not run by ci.yml", result.stderr)
            dispatcher.write_text(original_dispatcher)

            for filename in ("check-synthetic.sh", "test-check-synthetic.sh", "test-synthetic.sh"):
                with self.subTest(filename=filename):
                    added = scripts / filename
                    added.touch()
                    result = audit(original)
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn(f"{filename} is not run by verify.sh", result.stderr)
                    added.unlink()
            (scripts / "verify.sh").write_text((scripts / "verify.sh").read_text().replace(
                "            bash scripts/test-touched-crates.sh\n", ""))
            result = audit(original)
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("test-touched-crates.sh is not run by verify.sh", result.stderr)

    def test_touched_crates_includes_uncommitted_changes(self):
        helper = WORKFLOW.parent.parent.parent / "scripts/touched-crates.sh"
        with tempfile.TemporaryDirectory(prefix="carina-touched-crates-") as temporary:
            root = Path(temporary)
            # Metadata is a recording fixture: this test still exercises real
            # Git diffs, but never invokes Cargo on the host.
            binary = root / "bin/cargo"
            binary.parent.mkdir()
            metadata = root / "metadata.json"
            metadata.write_text(json.dumps({
                "workspace_root": str(root), "workspace_members": ["cli", "core"],
                "packages": [
                    {"id": "cli", "name": "carina-cli", "dependencies": [],
                     "manifest_path": str(root / "carina-cli/Cargo.toml")},
                    {"id": "core", "name": "carina-core", "dependencies": [],
                     "manifest_path": str(root / "carina-core/Cargo.toml")},
                ],
            }))
            binary.write_text('#!/bin/sh\ncat "$TOUCHED_METADATA"\n')
            binary.chmod(0o755)
            environment = dict(os.environ, PATH=f"{binary.parent}:{os.environ['PATH']}",
                               TOUCHED_METADATA=str(metadata))

            def git(*args):
                subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True)

            git("init", "--quiet")
            tracked = root / "carina-cli/src/lib.rs"
            tracked.parent.mkdir(parents=True)
            tracked.write_text("// initial\n")
            git("add", ".")
            git("-c", "user.name=Verify Test", "-c", "user.email=verify@example.invalid",
                "commit", "--quiet", "-m", "Fixture baseline")

            def scope():
                return subprocess.check_output(["bash", str(helper), "--base", "HEAD"],
                                               cwd=root, env=environment, text=True).strip()

            self.assertEqual(scope(), "")
            tracked.write_text("// unstaged\n")
            self.assertEqual(scope(), "-p carina-cli")
            git("add", ".")
            self.assertEqual(scope(), "-p carina-cli")
            untracked = root / "carina-core/src/untracked.rs"
            untracked.parent.mkdir(parents=True)
            untracked.touch()
            self.assertEqual(scope(), "--workspace")

    def test_only_the_aggregate_is_named_test(self):
        self.assertEqual({job for job, block in self.jobs.items() if field(block, "name") == "Test"}, {"test-result"})
        condition = field(self.jobs["test-result"], "if")
        self.assertIn("always()", condition)
        for event in ("pull_request", "merge_group"):
            for status in ("failure", "cancelled", "skipped"):
                self.assertTrue(selected(condition, event, False, False, status))

    def test_aggregate_fails_for_each_unsuccessful_dependency(self):
        block = self.jobs["test-result"]
        dependencies = {name.strip() for name in field(block, "needs").strip("[]").split(",")}
        self.assertEqual(dependencies, {"changes", "test"} | TEST_JOBS)
        # Execute the exact workflow program, including a failure/skipped
        # matrix result. Every single unsuccessful dependency must fail Test.
        program = textwrap.dedent(block.split("        run: |\n", 1)[1])
        def run(results):
            return subprocess.run(["bash", "-e", "-c", program],
                                  env=dict(os.environ, NEEDS_RESULTS=json.dumps(results)),
                                  capture_output=True, text=True)
        successful = {job: {"result": "success"} for job in dependencies}
        self.assertEqual(run(successful).returncode, 0)
        for job in dependencies:
            for status in ("failure", "cancelled", "skipped"):
                with self.subTest(job=job, status=status):
                    results = dict(successful, **{job: {"result": status}})
                    outcome = run(results)
                    self.assertNotEqual(outcome.returncode, 0)
                    self.assertIn(job, outcome.stderr)
                    self.assertIn(status, outcome.stderr)

    def test_setup_keeps_shared_compiled_cache_inside_each_disposable_home(self):
        repo = WORKFLOW.parent.parent.parent
        source = (repo / "scripts/setup-nextest-user-dirs.sh").read_text()
        with tempfile.TemporaryDirectory(prefix="carina-ci-setup-") as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            script = root / "scripts/setup-nextest-user-dirs.sh"
            script.write_text(source)
            homes = []
            for number in range(2):
                env_file = root / f"env-{number}"
                result = subprocess.run(["sh", str(script)],
                                        env=dict(os.environ, NEXTEST_ENV=str(env_file), CARGO_TARGET_DIR=str(root / "target")),
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = dict(line.split("=", 1) for line in env_file.read_text().splitlines())
                test_home = Path(environment["CARINA_TEST_HOME"])
                self.assertEqual(Path(environment["HOME"]), test_home)
                self.assertEqual(Path(environment["TMPDIR"]), test_home / "tmp")
                self.assertEqual(Path(environment["CARINA_TEST_WASM_CACHE_DIR"]), test_home / "shared-compiled-wasm")
                shared_cache = Path(environment["CARINA_TEST_WASM_CACHE_DIR"])
                self.assertTrue(shared_cache.is_dir())
                (shared_cache / "shared.cwasm").write_bytes(b"shared compiled code")
                self.assertFalse(list((test_home / ".carina/cache").iterdir()))
                homes.append(test_home)
            self.assertNotEqual(*homes)

    def test_fixture_guard_rejects_each_missing_or_empty_fixture(self):
        repo = WORKFLOW.parent.parent.parent
        source = (repo / "scripts/require-wasm-fixtures.sh").read_text()
        fixtures = ["carina-provider-mock", "carina-provider-mock-raw-float",
                    "carina-provider-mock-http", "carina-provider-mock-http-aws",
                    "carina-provider-mock-http-aws-unused", "carina-provider-mock-http-sdk-only"]
        with tempfile.TemporaryDirectory(prefix="carina-ci-fixtures-") as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            script = root / "scripts/require-wasm-fixtures.sh"
            script.write_text(source)
            target = root / "target/wasm32-wasip2/debug"
            target.mkdir(parents=True)
            for fixture in fixtures:
                (target / (fixture + ".wasm")).write_bytes(b"fixture")
            def run():
                return subprocess.run(["bash", str(script)], capture_output=True, text=True)
            self.assertEqual(run().returncode, 0)
            for fixture in fixtures:
                path = target / (fixture + ".wasm")
                for empty in (False, True):
                    with self.subTest(fixture=fixture, empty=empty):
                        if empty:
                            path.touch()
                        else:
                            path.unlink()
                        result = run()
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn(fixture, result.stderr)
                path.write_bytes(b"fixture")


if __name__ == "__main__":
    unittest.main()
