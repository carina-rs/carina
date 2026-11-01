#!/usr/bin/env bash
# Keep workspace members on the shared wit-bindgen version so fixtures cannot
# drift from the SDK and hide component binding regressions.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

cargo metadata --no-deps --format-version 1 --offline |
    python3 -c '
import json
from pathlib import Path
import sys
import tomllib

metadata = json.load(sys.stdin)
root_manifest = Path(metadata["workspace_root"]) / "Cargo.toml"
with root_manifest.open("rb") as manifest_file:
    root = tomllib.load(manifest_file)

failed = False
if "wit-bindgen" not in root.get("workspace", {}).get("dependencies", {}):
    print("ERROR: [workspace.dependencies] must declare wit-bindgen.")
    failed = True


def check_dependencies(member, table, dependencies):
    global failed
    for name, declaration in dependencies.items():
        if name != "wit-bindgen" and not (
            isinstance(declaration, dict) and declaration.get("package") == "wit-bindgen"
        ):
            continue
        if not isinstance(declaration, dict) or declaration.get("workspace") is not True:
            print(f"ERROR: {member} [{table}]: {name} must use workspace = true.")
            failed = True


workspace_members = set(metadata["workspace_members"])
dependency_tables = ("dependencies", "dev-dependencies", "build-dependencies")
for package in metadata["packages"]:
    if package["id"] not in workspace_members:
        continue
    with Path(package["manifest_path"]).open("rb") as manifest_file:
        manifest = tomllib.load(manifest_file)
    for table in dependency_tables:
        check_dependencies(package["name"], table, manifest.get(table, {}))
    for target, tables in manifest.get("target", {}).items():
        for table in dependency_tables:
            check_dependencies(
                package["name"],
                f"target.{json.dumps(target)}.{table}",
                tables.get(table, {}),
            )

if failed:
    sys.exit(1)

print("OK: All workspace wit-bindgen dependencies use workspace = true.")
'
