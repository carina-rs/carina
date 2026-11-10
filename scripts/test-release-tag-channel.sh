#!/usr/bin/env bash
# Check release channels without building binaries or publishing releases.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 - "$repo_root/scripts/release-tag-channel.sh" <<'PY'
import subprocess
import sys

script = sys.argv[1]
for tag, channel in (
    ("v0.14.0", "stable"),
    ("v10.2.33", "stable"),
    ("v0.14.0-rc.1", "rc"),
    ("v1.2.3-rc.12", "rc"),
):
    result = subprocess.run(["bash", script, tag], capture_output=True, text=True)
    assert result.returncode == 0, (tag, result.stderr)
    assert result.stdout == channel + "\n", (tag, result.stdout)
    assert result.stderr == "", (tag, result.stderr)

for tag in (
    "v0.14.0-beta.1",
    "v0.14.0-rc",
    "v0.14.0-rc.x",
    "v0.14",
    "0.14.0",
    "v0.14.0-rc.1-extra",
    "vfoo",
    "",
):
    result = subprocess.run(["bash", script, tag], capture_output=True, text=True)
    assert result.returncode != 0, tag
    assert result.stdout == "", (tag, result.stdout)
    assert "::error::" in result.stderr, (tag, result.stderr)
    assert "vX.Y.Z" in result.stderr and "vX.Y.Z-rc.N" in result.stderr, result.stderr

print("OK: stable and rc release tags are classified; unsupported tags are rejected.")
PY
