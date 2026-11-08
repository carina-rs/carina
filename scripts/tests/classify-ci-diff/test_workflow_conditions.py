"""Pin CI gating and main-branch cache writers without running any Rust tools."""

import ast
from pathlib import Path
import re
import sys
import unittest

WORKFLOW = Path(sys.argv.pop(1))
CODE_JOBS = {
    "check", "feature-isolation", "fmt", "clippy", "check-examples",
    "validate-crn", "check-example-warnings", "test", "binary-build",
}
CACHE_JOBS = CODE_JOBS - {"fmt"}
SCRIPT_JOBS = {
    "provider-boundaries", "error-source-display", "no-wall-clock-test-assertions",
    "docs-spellings", "check-resources-access", "check-cli-hints",
    "check-no-home-env-reads", "check-crates-not-publishable",
    "check-wit-bindgen-workspace", "check-plan-fixtures",
    "check-rust-cache-via-local-action", "check-vendored-patches-used",
}
ALL_JOBS = CODE_JOBS | SCRIPT_JOBS | {"changes"}


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


def selected(condition, event, code, version_only):
    if condition is None:
        return True
    values = {
        "github.event_name": event,
        "needs.changes.outputs.code": str(code).lower(),
        "needs.changes.outputs.version_only": str(version_only).lower(),
    }
    for key, value in values.items():
        condition = condition.replace(key, repr(value))
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
            ("main push", "push", True, False, CACHE_JOBS | {"changes"}),
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
        for name in cached:
            with self.subTest(job=name):
                self.assertTrue(selected(field(self.jobs[name], "if"), "push", True, False))

    def test_version_only_gate_applies_only_to_pull_requests(self):
        for name in CODE_JOBS - {"binary-build"}:
            for event in ("merge_group", "push"):
                with self.subTest(job=name, event=event):
                    self.assertEqual(selected(field(self.jobs[name], "if"), event, True, True), name != "fmt" or event != "push")

    def test_push_only_compiles_test_artifacts(self):
        def commands(event):
            result = []
            for step in re.split(r"(?m)(?=^      - )", self.jobs["test"]):
                prefix = r"(?:      - |        )"
                run = field(step, "run", prefix)
                if run is None or not selected(field(step, "if", prefix), event, True, False):
                    continue
                if run == ">-":
                    run = " ".join(re.findall(r"^          (.*)$", step, re.MULTILINE))
                self.assertNotIn(run, ("|", ">"), "update the test for new multiline run syntax")
                result.append(run)
            return result

        build = "cargo build -p carina-provider-mock -p carina-provider-mock-http --target wasm32-wasip2"
        compile_tests = "cargo nextest run --workspace --all-features --no-run"
        self.assertEqual(commands("push"), [build, compile_tests])
        full_suite = [
            build, compile_tests, "cargo nextest run --workspace --all-features",
            "scripts/test-in-process-isolation.sh", "bash scripts/check-provider-cause-test-api.sh",
            "cargo test --workspace --doc --all-features",
        ]
        self.assertEqual(commands("merge_group"), full_suite)
        self.assertEqual(commands("pull_request"), full_suite)


if __name__ == "__main__":
    unittest.main()
