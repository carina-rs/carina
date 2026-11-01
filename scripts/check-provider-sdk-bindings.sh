#!/usr/bin/env bash
# Providers use SDK-owned bindings, never their own generator or WIT checkout.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

cargo metadata --no-deps --format-version 1 --offline |
    python3 -c '
import json
from pathlib import Path
import sys

metadata = json.load(sys.stdin)
members = set(metadata["workspace_members"])
failed = False
for package in metadata["packages"]:
    # A dev-only SDK dependency does not make a host crate a provider.
    if package["id"] not in members or not any(
        dependency["name"] == "carina-plugin-sdk" and dependency["kind"] is None
        for dependency in package["dependencies"]
    ):
        continue
    name = package["name"]
    # Cargo normalizes renamed, workspace-inherited and target-specific deps.
    for dependency in package["dependencies"]:
        dependency_name = dependency["name"]
        if dependency_name.startswith(("wit-bindgen", "carina-plugin-bindings-")):
            print(f"ERROR: {name} directly depends on {dependency_name}; use carina-plugin-sdk.")
            failed = True
    crate_dir = Path(package["manifest_path"]).parent
    for path in sorted(crate_dir.rglob("*")):
        if not path.is_file() or path.suffix not in {".rs", ".toml", ".sh", ".yml", ".yaml"}:
            continue
        for line_number, line in enumerate(path.read_text().splitlines(), 1):
            if "carina-plugin-wit" in line or "__carina_wit_bindgen" in line:
                print(f"ERROR: {path}:{line_number}: provider-owned WIT reference; use carina-plugin-sdk.")
                failed = True

if failed:
    sys.exit(1)
print("OK: All workspace providers use SDK-owned bindings without direct binding dependencies or WIT paths.")
'
