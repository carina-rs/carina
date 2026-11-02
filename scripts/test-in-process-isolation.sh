#!/usr/bin/env bash
# Run native CLI/mock suites concurrently inside their libtest processes.
# Cargo compilation keeps the developer's environment; only test children get
# the disposable HOME/TMPDIR produced by the nextest setup contract.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import uuid

limit = int(os.environ.get("CARINA_ISOLATION_TIMEOUT", "1200"))

def run(args, fail_fast=True, **kwargs):
    child = subprocess.Popen(args, start_new_session=True, **kwargs)
    try:
        stdout, stderr = child.communicate(timeout=limit)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.communicate()
        message = f"in-process isolation check timed out after {limit}s: {args[0]}"
        if fail_fast:
            raise SystemExit(message)
        print(message, flush=True)
        return 124
    if child.returncode and fail_fast:
        if stdout:
            print(stdout)
        raise SystemExit(child.returncode)
    return stdout if fail_fast else child.returncode

listing = run([
    os.environ.get("CARGO", "cargo"), "nextest", "list",
    "-p", "carina-cli", "-p", "carina-provider-mock", "--lib", "--bins",
    "--all-features",
    "--message-format", "json", "--list-type", "binaries-only",
], stdout=subprocess.PIPE, text=True)
metadata = json.loads(listing)
expected = {("carina_cli", "lib"), ("carina_provider_mock", "lib"), ("carina", "bin")}
binaries = {}
for binary in metadata["rust-binaries"].values():
    key = (binary["binary-name"].replace("-", "_"), binary["kind"])
    if key in expected:
        binaries[key] = binary["binary-path"]
if binaries.keys() != expected:
    raise SystemExit(f"missing in-process test binaries: {expected - binaries.keys()}")

with tempfile.TemporaryDirectory(prefix="carina-in-process-env-") as temporary:
    env_file = Path(temporary) / "nextest-env"
    env_file.touch()
    setup_env = dict(os.environ, NEXTEST_ENV=str(env_file))
    run(["scripts/setup-nextest-user-dirs.sh"], env=setup_env)
    child_env = dict(os.environ)
    for line in env_file.read_text().splitlines():
        key, value = line.split("=", 1)
        child_env[key] = value
    # Keep the production guard active just as nextest does for test children.
    child_env["NEXTEST_RUN_ID"] = str(uuid.uuid4())
    # Insta must resolve checked-in snapshots without asking Cargo to discover
    # its workspace after HOME has been replaced with a disposable directory.
    workspace = Path.cwd()
    child_env["INSTA_WORKSPACE_ROOT"] = str(workspace)
    test_home = Path(child_env["CARINA_TEST_HOME"])
    if Path(child_env["HOME"]) != test_home or Path(child_env["TMPDIR"]) != test_home / "tmp":
        raise SystemExit("nextest setup did not provide the HOME/TMPDIR isolation contract")
    failures = []
    try:
        for key, executable in sorted(binaries.items()):
            print(f"Running {key[0]} ({key[1]}) with --test-threads=8", flush=True)
            package = "carina-provider-mock" if key[0] == "carina_provider_mock" else "carina-cli"
            binary_env = dict(child_env, CARGO_MANIFEST_DIR=str(workspace / package))
            args = [executable, "--test-threads=8"]
            if key == ("carina_cli", "lib"):
                # This test calls std::env::set_var (unsafe in edition 2024):
                # env() still reads the process environment. set_var races
                # with threads reading environ outside Rust's env lock, such
                # as libc getaddrinfo, timezone lookup, and C dependencies, so
                # it cannot share a process with other threads. It still runs
                # under nextest's process-per-test isolation. Remove this sole
                # exclusion when env() takes an explicit EnvironmentSnapshot
                # after carina#4189. --exact also makes --skip match only this name.
                args += ["--skip", "display::tests::plan_export_shows_env_plaintext", "--exact"]
            status = run(args, env=binary_env, cwd=workspace / package, fail_fast=False)
            if status:
                failures.append((key, status))
    finally:
        shutil.rmtree(test_home.parent)
    if failures:
        raise SystemExit(f"in-process isolation check failed: {failures}")
PY
