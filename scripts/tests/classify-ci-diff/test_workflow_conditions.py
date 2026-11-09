"""Pin CI gating and main-branch cache writers without running any Rust tools."""

import ast
import json
import os
from pathlib import Path
import re
import sys
import subprocess
import tempfile
import textwrap
from types import SimpleNamespace
import unittest

WORKFLOW = Path(sys.argv.pop(1))
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

    def test_push_only_compiles_test_artifacts(self):
        build = "cargo build -p carina-provider-mock -p carina-provider-mock-http --target wasm32-wasip2"
        archive = "cargo nextest archive --workspace --all-features --archive-file nextest-archive.tar.zst"
        version = "rustc --version | awk '{print \"version=\" $2}' >> \"$GITHUB_OUTPUT\""
        self.assertEqual(self.commands("test", "push"), [version, build, "cargo nextest run --workspace --all-features --no-run"])
        for event in ("merge_group", "pull_request"):
            self.assertEqual(self.commands("test", event), [version, build, archive])
        self.assertEqual(self.commands("test-doctests", "pull_request"), ["cargo test --workspace --doc --all-features"])
        self.assertNotIn("shared-key:", self.jobs["test"])
        self.assertNotIn("cache-job:", self.jobs["test"])
        for name in AUX_CACHE_JOBS:
            self.assertIn("cache-job: test", self.jobs[name])
        action = (WORKFLOW.parent.parent / "actions/rust-cache/action.yml").read_text()
        self.assertIn("format('{0}-{1}', steps.manifest.outputs.key, inputs.cache-job)", action)

    def test_provider_cause_fetches_before_offline_checks(self):
        self.assertEqual(self.commands("test-provider-cause", "pull_request"),
                         ["cargo fetch --locked", "bash scripts/check-provider-cause-test-api.sh"])

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
        assignment = next(node for node in ast.parse(program).body
                          if isinstance(node, ast.Assign)
                          and any(isinstance(target, ast.Name) and target.id == "build_args"
                                  for target in node.targets))
        # Inspect only argument selection. Never execute isolation, Cargo or
        # native test binaries from this workflow self-check.
        expression = compile(ast.Expression(assignment.value), "isolation build_args", "eval")
        local = eval(expression, {"sys": SimpleNamespace(argv=["isolation"])})
        archived = eval(expression, {"sys": SimpleNamespace(argv=["isolation", "fixtures.tar.zst"])})
        self.assertEqual(local, ["--workspace", "--all-features"])
        self.assertEqual(archived, ["--archive-file", "fixtures.tar.zst", "--workspace-remap", ".",
                                    "--extract-to", ".", "--extract-overwrite"])

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
        self.assertEqual(command, "cargo nextest run --archive-file nextest-archive.tar.zst --workspace-remap . --extract-to . --extract-overwrite --partition count:${{ matrix.partition }}/6 --no-fail-fast")
        self.assertEqual(self.commands("test-isolation", "pull_request")[1], "scripts/test-in-process-isolation.sh nextest-archive.tar.zst")

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
