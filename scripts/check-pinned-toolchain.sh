#!/usr/bin/env bash
set -euo pipefail
repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
python3 - "$repo_root" <<'PY'
from pathlib import Path
import re
import subprocess
import sys
import tomllib

root = Path(sys.argv[1])
failed = False
try:
    with (root / "rust-toolchain.toml").open("rb") as file:
        channel = tomllib.load(file).get("toolchain", {}).get("channel")
    if not isinstance(channel, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", channel):
        raise ValueError("[toolchain].channel must be an exact X.Y.Z version")
except (OSError, ValueError, AttributeError) as error:
    print(f"ERROR: rust-toolchain.toml: {error}", file=sys.stderr)
    failed = True

try:
    files = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
                                    cwd=root, stderr=subprocess.PIPE, text=True, timeout=10).split("\0")
    for filename in sorted(set(files)):
        if Path(filename).name in {"rust-toolchain", "rust-toolchain.toml"} and filename != "rust-toolchain.toml":
            print(f"ERROR: {filename}: only the root rust-toolchain.toml may select the toolchain.", file=sys.stderr)
            failed = True
except (OSError, subprocess.SubprocessError) as error:
    print(f"ERROR: cannot check additional toolchain files with git ls-files: {error}", file=sys.stderr)
    failed = True

patterns = (
    r"dtolnay/rust-toolchain|actions-rs/toolchain", r"(?<![\w-])['\"]?toolchain['\"]?\s*:",
    r"RUSTUP_TOOLCHAIN", r"\bcargo\s+\+", r"\brustup\s+(default|override)\b",
    r"\brustup\s+(toolchain\s+)?install\s+[^\s#-]",
)
# ponytail: Shell continuations and folded scalars are not joined; add a parser if that ever matters.
for path in sorted((root / ".github").rglob("*")):
    if not path.is_file():
        continue
    for number, line in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        if line.lstrip().startswith("#") or not any(re.search(pattern, line) for pattern in patterns):
            continue
        print(f"ERROR: {path.relative_to(root)}:{number}: use rust-toolchain.toml instead of {line.strip()}", file=sys.stderr)
        failed = True
if failed:
    sys.exit(1)
print("OK: Rust is pinned in rust-toolchain.toml and CI has no per-job toolchains.")
PY
