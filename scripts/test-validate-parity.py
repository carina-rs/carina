#!/usr/bin/env python3
"""Exercise sweep coverage accounting without launching the corpus binaries."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("validate_parity", Path(__file__).with_name("validate-parity.py"))
PARITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PARITY)


class ValidationCoverageTests(unittest.TestCase):
    def sweep(self, responses):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directories = [root / name for name in responses]
            for directory in directories:
                directory.mkdir(parents=True)
                (directory / "main.crn").write_text("exports {}\n")
            evidence = root / "evidence"

            def invoke(command, **_):
                response = responses[Path(command[2]).relative_to(root).as_posix()]
                stdout = response if isinstance(response, bytes) else json.dumps(response).encode()
                return subprocess.CompletedProcess(command, 1, stdout, b"")

            arguments = ["validate-parity.py", "--binary", str(root / "carina"),
                         "--repeat", "8", "--repo", str(root), "--evidence", str(evidence)]
            with patch.object(sys, "argv", arguments), \
                    patch.object(PARITY, "configuration_directories", return_value=directories), \
                    patch.object(PARITY, "file_sha256", return_value="fixture"), \
                    patch.object(PARITY, "git_revision", return_value="fixture"), \
                    patch.object(PARITY.subprocess, "run", side_effect=invoke), \
                    contextlib.redirect_stdout(io.StringIO()):
                status = PARITY.main()
            return status, json.loads((evidence / "summary.json").read_text())

    def test_provider_setup_failures_do_not_count_as_validation(self):
        status, summary = self.sweep({
            "validated": {"status": "error", "resources": [], "errors": [{"message": "Unknown attribute 'zulu'"}]},
            "unavailable": {"status": "error", "resources": [], "errors": [{"message": "Provider 'mock' not installed. Run `carina init`"}]},
        })
        self.assertEqual(status, 0)
        self.assertEqual(summary.get("validation_directories"), 1)
        self.assertEqual(summary.get("provider_not_installed_directories"), 1)
        self.assertEqual(summary.get("setup_failure_directories"), 1)

    def test_entire_shard_stopping_at_provider_setup_fails(self):
        status, summary = self.sweep({
            "unavailable": {"status": "error", "resources": [], "errors": [{"message": "Provider 'mock' not installed"}]},
        })
        self.assertEqual(status, 1)
        self.assertEqual(summary.get("validation_directories"), 0)
        self.assertEqual(summary.get("provider_not_installed_directories"), 1)

    def test_unstructured_process_failure_does_not_count_as_validation(self):
        status, summary = self.sweep({"failed": b"process setup failed\n"})
        self.assertEqual(status, 1)
        self.assertEqual(summary.get("validation_directories"), 0)
        self.assertEqual(summary.get("setup_failure_directories"), 1)

    def test_every_named_diagnostic_order_fixture_must_reach_validation(self):
        status, summary = self.sweep({
            "ordinary": {"status": "ok", "resources": [], "errors": []},
            "diagnostic_order/focused": {"status": "error", "resources": [], "errors": [{"message": "Provider 'mock' not installed"}]},
        })
        self.assertEqual(summary["validation_directories"], 1)
        self.assertEqual(status, 1)
        self.assertEqual(len(summary.get("unvalidated_diagnostic_order_directories", [])), 1)

    def test_named_invalid_fixture_still_counts_as_validation(self):
        status, summary = self.sweep({
            "diagnostic_order/focused": {"status": "error", "resources": [], "errors": [{"message": "Unknown argument 'zulu'"}]},
        })
        self.assertEqual(status, 0)
        self.assertEqual(summary.get("validated_directory_names"), summary.get("diagnostic_order_directory_names"))
        self.assertEqual(len(summary.get("validated_directory_names", [])), 1)


if __name__ == "__main__":
    unittest.main()
