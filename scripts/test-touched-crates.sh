#!/usr/bin/env bash
# Exercise scope selection against synthetic Cargo metadata, without Rust tools.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 - "$repo_root/scripts/touched-crates.sh" <<'PY'
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

HELPER = Path(sys.argv.pop(1)).resolve()


class TouchedCratesTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="carina-scope-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        binary = self.root / "bin/cargo"
        binary.parent.mkdir()
        binary.write_text(
            '#!/bin/sh\n'
            '[ "$*" = "metadata --format-version 1 --no-deps" ] || exit 2\n'
            'echo metadata >> "$METADATA_TRACE"\n'
            '[ "$METADATA_FAIL" != 1 ] || exit 1\n'
            'cat "$METADATA_FIXTURE"\n'
        )
        binary.chmod(0o755)
        self.metadata_file = self.root / "metadata.json"
        self.trace = self.root / "trace"
        self.env = dict(os.environ, PATH=f"{binary.parent}:{os.environ['PATH']}",
                        METADATA_FIXTURE=str(self.metadata_file), METADATA_FAIL="0",
                        METADATA_TRACE=str(self.trace), GIT_CONFIG_NOSYSTEM="1",
                        GIT_CONFIG_GLOBAL=os.devnull)
        self.packages = []
        for name, dependencies in [
            ("carina-plugin-sdk", []),
            ("carina-provider-example", [("carina-plugin-sdk", None)]),
            ("carina-plugin-host", [("carina-provider-example", "dev")]),
            ("carina-cli", [("carina-plugin-host", "build")]),
            ("carina-tui", []),
        ]:
            self.packages.append({
                "id": name, "name": name,
                "manifest_path": str(self.root / name / "Cargo.toml"),
                "dependencies": [self.dependency(target, kind) for target, kind in dependencies],
            })

    def dependency(self, directory, kind=None, **fields):
        return dict(name="renamed-dependency", path=str(self.root / directory), kind=kind, **fields)

    def scope(self, *files):
        self.metadata_file.write_text(json.dumps({
            "workspace_root": str(self.root),
            "workspace_members": [package["id"] for package in self.packages],
            "packages": self.packages,
        }))
        result = subprocess.run(["bash", str(HELPER), "--stdin"], cwd=self.root,
                                input="\n".join(files), env=self.env,
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def test_transitive_reverse_dependencies_include_dev_and_build_edges(self):
        self.assertEqual(self.scope("carina-plugin-sdk/src/lib.rs"),
                         "-p carina-cli -p carina-plugin-host -p carina-plugin-sdk -p carina-provider-example")
        self.assertEqual(self.scope("carina-provider-example/src/lib.rs"),
                         "-p carina-cli -p carina-plugin-host -p carina-provider-example")
        self.assertEqual(self.scope("carina-cli/src/lib.rs"), "-p carina-cli")

    def test_leaf_scope_and_deduplication_are_deterministic(self):
        self.assertEqual(self.scope("carina-tui/src/lib.rs", "carina-tui/src/lib.rs"), "-p carina-tui")
        self.assertEqual(self.scope("carina-tui/README.md"), "-p carina-tui")
        self.assertEqual(self.scope("carina-tui/src/lib.rs", "carina-provider-example/src/lib.rs"),
                         "-p carina-cli -p carina-plugin-host -p carina-provider-example -p carina-tui")

    def test_full_closure_becomes_workspace(self):
        self.assertEqual(self.scope("carina-plugin-sdk/src/lib.rs", "carina-tui/src/lib.rs"), "--workspace")

    def test_cycles_terminate(self):
        self.packages[0]["dependencies"].append(self.dependency("carina-plugin-host", "dev"))
        self.assertEqual(self.scope("carina-provider-example/src/lib.rs"),
                         "-p carina-cli -p carina-plugin-host -p carina-plugin-sdk -p carina-provider-example")

    def test_optional_and_target_specific_edges_are_included(self):
        self.packages[-1]["dependencies"].append(
            self.dependency("carina-plugin-sdk", optional=True, target='cfg(target_arch = "wasm32")'))
        self.assertEqual(self.scope("carina-plugin-sdk/src/lib.rs"), "--workspace")

    def test_member_names_and_paths_come_from_metadata(self):
        self.packages.append({
            "id": "new-member", "name": "new-member",
            "manifest_path": str(self.root / "nested/new-member/Cargo.toml"),
            "dependencies": [self.dependency("carina-tui", "dev")],
        })
        self.assertEqual(self.scope("carina-tui/src/lib.rs"), "-p carina-tui -p new-member")
        self.assertEqual(self.scope("nested/new-member/src/lib.rs"), "-p new-member")
        # A path dependency outside the workspace must not become a member.
        self.packages[0]["dependencies"].append(self.dependency("vendor/helper"))
        self.assertEqual(self.scope("carina-plugin-sdk/src/lib.rs"),
                         "-p carina-cli -p carina-plugin-host -p carina-plugin-sdk -p carina-provider-example")

    def test_fallbacks_remain_workspace(self):
        for filename in ("carina-core/src/lib.rs", "Cargo.toml", "Cargo.lock",
                         "carina-tui/Cargo.toml", "carina-tui/Cargo.lock", "unknown/source.rs"):
            with self.subTest(filename=filename):
                self.assertEqual(self.scope(filename), "--workspace")

    def test_wasm_fixture_changes_include_consumers_without_cargo_edges(self):
        for crate in ("carina-provider-mock", "carina-provider-mock-http"):
            self.packages.append({
                "id": crate, "name": crate, "dependencies": [],
                "manifest_path": str(self.root / crate / "Cargo.toml"),
            })
            for filename in (crate + "/src/lib.rs", crate + "/src/bin/fixture.rs"):
                with self.subTest(filename=filename):
                    self.assertEqual(self.scope(filename), "--workspace")

    def test_cargo_execution_definitions_require_workspace(self):
        for filename in (
            "scripts/verify.sh", "scripts/touched-crates.sh",
            "scripts/test-in-process-isolation.sh", "scripts/setup-nextest-user-dirs.sh",
            "scripts/require-wasm-fixtures.sh", "scripts/wasm-fixture-crates.txt",
            ".config/nextest.toml", ".config/nested/new-config.toml",
            ".cargo/config.toml", ".cargo/nested/new-config.toml",
            ".github/workflows/ci.yml", ".github/actions/rust-cache/action.yml",
            ".github/actions/nested/setup/helper.py",
        ):
            with self.subTest(filename=filename):
                self.assertEqual(self.scope(filename), "--workspace")
        self.assertFalse(self.trace.exists())

    def test_empty_and_ignored_changes_do_not_run_cargo(self):
        self.assertEqual(self.scope(), "")
        for filename in ("README.md", "CLAUDE.md", "scripts/check-plan-fixtures.sh",
                         ".github/ISSUE_TEMPLATE/bug.yml", "site/src/index.ts",
                         "notes/example.txt", "infra/example.crn"):
            with self.subTest(filename=filename):
                self.assertEqual(self.scope(filename), "")
        self.assertFalse(self.trace.exists())

    def test_metadata_failure_falls_back_to_workspace(self):
        self.env["METADATA_FAIL"] = "1"
        self.assertEqual(self.scope("carina-tui/src/lib.rs"), "--workspace")

    def test_incomplete_metadata_falls_back_to_workspace(self):
        self.packages = []
        self.assertEqual(self.scope("carina-tui/src/lib.rs"), "--workspace")

    def test_each_git_discovery_failure_is_fatal(self):
        binary = self.root / "bin/git"
        binary.write_text(
            '#!/bin/sh\n'
            'if [ "$*" = "$GIT_FAIL_COMMAND" ]; then\n'
            '  echo "synthetic git failure" >&2\n'
            '  exit 74\n'
            'fi\n'
        )
        binary.chmod(0o755)
        for command, message in (
            ("diff --name-only base...HEAD", "git diff base...HEAD failed"),
            ("diff --name-only HEAD", "git diff HEAD failed"),
            ("ls-files --others --exclude-standard", "git ls-files failed"),
        ):
            with self.subTest(command=command):
                result = subprocess.run(
                    ["bash", str(HELPER), "--base", "base"], cwd=self.root,
                    env=dict(self.env, GIT_FAIL_COMMAND=command),
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertIn(message, result.stderr)
        self.assertFalse(self.trace.exists())

    def test_no_merge_base_fails_scope_and_local_verify(self):
        def git(*args):
            return subprocess.check_output(
                ["git", "-c", "user.name=Scope fixture", "-c",
                 "user.email=scope@example.invalid", *args], cwd=self.root,
                env=self.env, stderr=subprocess.PIPE, text=True, timeout=10,
            ).strip()

        git("init", "-q", "--template=")
        # Keep the synthetic tools and copied scripts out of the changed files.
        (self.root / ".git/info").mkdir(exist_ok=True)
        (self.root / ".git/info/exclude").write_text("/bin/\n/scripts/\n/metadata.json\n/trace\n")
        git("commit", "-qm", "base", "--allow-empty")
        base = git("rev-parse", "HEAD")
        git("checkout", "-q", "--orphan", "unrelated")
        git("commit", "-qm", "unrelated", "--allow-empty")
        self.assertEqual(git("status", "--porcelain"), "")

        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("touched-crates.sh", "verify.sh"):
            shutil.copyfile(HELPER.parent / name, scripts / name)
        for command in (
            ["bash", str(HELPER), "--base", base],
            ["bash", str(scripts / "verify.sh"), "--local", "--base", base,
             "check", "nextest", "clippy", "doctests"],
        ):
            with self.subTest(command=command):
                result = subprocess.run(
                    command, cwd=self.root, env=self.env,
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertIn("no merge base", result.stderr)
                self.assertIn(f"touched-crates.sh: git diff {base}...HEAD failed", result.stderr)
        self.assertFalse(self.trace.exists())


unittest.main()
PY
