#!/usr/bin/env bash
# Exercise the pinned-toolchain guard with both valid and regressed fixtures.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 - "$repo_root" <<'PY'
from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

REPO = Path(sys.argv.pop(1)).resolve()
CHECKER = REPO / "scripts/check-pinned-toolchain.sh"
BASH = shutil.which("bash")


class PinnedToolchainTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="carina-pinned-toolchain-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.pin = self.root / "rust-toolchain.toml"
        self.pin.write_text('[toolchain]\nchannel = "1.2.3"\n')
        self.workflow = self.root / ".github/workflows/ci.yml"
        self.workflow.parent.mkdir(parents=True)
        self.workflow.write_text("steps:\n  - uses: ./.github/actions/rust-toolchain\n")
        self.environment = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                                GIT_OPTIONAL_LOCKS="0")
        self.git("init", "-q", "--template=")
        (self.root / ".gitignore").write_text("/target/\n/.worktrees/\n")
        self.git("add", "--", "rust-toolchain.toml", ".gitignore")

    def git(self, *arguments):
        return subprocess.check_output(["git", *arguments], cwd=self.root, env=self.environment,
                                       stderr=subprocess.PIPE, text=True, timeout=10)

    def guard(self, error=None, root=None):
        result = subprocess.run(["bash", str(CHECKER), str(root or self.root)],
                                cwd=REPO, env=self.environment,
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0 if error is None else 1,
                         result.stdout + result.stderr)
        if error is not None:
            self.assertIn(error, result.stderr)

    def test_repository_uses_pinned_toolchain(self):
        self.guard(root=REPO)

    def test_bad_channel_fails(self):
        self.pin.write_text('[toolchain]\nchannel = "stable"\n')
        self.guard("[toolchain].channel must be an exact X.Y.Z version")

    def test_per_job_toolchain_rules_fail(self):
        for line in (
            "uses: dtolnay/rust-toolchain@stable",
            "uses: actions-rs/toolchain@v1",
            "toolchain: stable",
            "run: RUSTUP_TOOLCHAIN=nightly cargo check",
            "run: cargo +nightly check",
            "run: rustup default stable",
            "run: rustup override set nightly",
            "run: rustup toolchain install stable",
            "run: rustup install stable",
        ):
            with self.subTest(line=line):
                self.workflow.write_text(line + "\n")
                self.guard(".github/workflows/ci.yml:1:")

    def test_bare_install_is_allowed(self):
        self.workflow.write_text("steps:\n  - run: rustup toolchain install\n")
        self.guard()

    def test_yaml_comment_lines_are_ignored(self):
        self.workflow.write_text(
            "# install the toolchain: from rust-toolchain.toml\n"
            "  # dtolnay/rust-toolchain actions-rs/toolchain RUSTUP_TOOLCHAIN\n"
            "  # cargo +nightly; rustup default stable; rustup override set nightly\n"
            "\t# rustup toolchain install stable; rustup install stable\n"
        )
        self.guard()

    def test_additional_tracked_toolchain_files_fail(self):
        path = self.root / "nested/rust-toolchain.toml"
        path.parent.mkdir()
        path.write_text("nightly\n")
        self.git("add", "--", "nested/rust-toolchain.toml")
        self.guard("nested/rust-toolchain.toml")

    def test_additional_untracked_toolchain_files_fail(self):
        (self.root / "rust-toolchain").write_text("nightly\n")
        self.guard("rust-toolchain")

    def test_ignored_build_and_worktree_toolchain_files_pass(self):
        for directory in ("target/debug", ".worktrees/another-branch/nested"):
            for filename in ("rust-toolchain", "rust-toolchain.toml"):
                path = self.root / directory / filename
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("nightly\n")
        self.guard()

class RustToolchainActionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="carina-rust-toolchain-action-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.trace = self.root / "trace"
        action = (REPO / ".github/actions/rust-toolchain/action.yml").read_text()
        self.script = textwrap.dedent(action.split("      run: |\n", 1)[1])
        self.env = dict(os.environ, PATH=str(self.bin), EXTRA_TARGETS="",
                        RUSTUP_FIXTURE_INSTALL_STATUS="0",
                        RUST_ACTION_TRACE=str(self.trace))
        for name in ("rustup", "rustc"):
            binary = self.bin / name
            binary.write_text(
                '#!/bin/sh\n'
                f'printf "%s\\n" "{name} $*" >> "$RUST_ACTION_TRACE"\n'
                'if [ "$1" = --version ]; then\n'
                f'  printf "%s\\n" "{name} (fixture)"\n'
                'elif [ "$1 $2" = "toolchain install" ]; then\n'
                '  exit "$RUSTUP_FIXTURE_INSTALL_STATUS"\n'
                'fi\n'
            )
            binary.chmod(0o755)

    def run_action(self):
        return subprocess.run([BASH, "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", self.script],
                              cwd=self.root, env=self.env, capture_output=True, text=True, timeout=10)

    def test_installs_from_file_without_selecting_a_version(self):
        result = self.run_action()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), [
            "rustup toolchain install", "rustc --version",
        ])

    def test_extra_targets_accept_spaces_commas_and_newlines(self):
        for targets in ("aarch64-unknown-linux-gnu x86_64-apple-darwin",
                        "aarch64-unknown-linux-gnu,x86_64-apple-darwin",
                        " ,aarch64-unknown-linux-gnu, \nx86_64-apple-darwin, "):
            with self.subTest(targets=targets):
                self.env["EXTRA_TARGETS"] = targets
                result = self.run_action()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.trace.read_text().splitlines(), [
                    "rustup toolchain install",
                    "rustup target add -- aarch64-unknown-linux-gnu x86_64-apple-darwin",
                    "rustc --version",
                ])
                self.trace.unlink()

    def test_extra_targets_cannot_be_interpreted_as_rustup_options(self):
        self.env["EXTRA_TARGETS"] = "--toolchain nightly"
        result = self.run_action()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rustup target add -- --toolchain nightly", self.trace.read_text().splitlines())

    def test_missing_rustup_fails_at_installation(self):
        (self.bin / "rustup").unlink()
        result = self.run_action()
        self.assertEqual(result.returncode, 127, result.stderr)
        self.assertIn("rustup: command not found", result.stderr)
        self.assertFalse(self.trace.exists())

    def test_installation_failure_stops_the_action(self):
        self.env["RUSTUP_FIXTURE_INSTALL_STATUS"] = "42"
        result = self.run_action()
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), ["rustup toolchain install"])


unittest.main()
PY
