#!/usr/bin/env bash
# Check that Carina workspace packages cannot be published to crates.io.
#
# Carina is not published to crates.io. Every workspace package must have
# `publish = false`, normally inherited via `publish.workspace = true`.
set -euo pipefail

cargo metadata --no-deps --format-version 1 |
    python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
offenders = [
    package["name"]
    for package in metadata["packages"]
    if package["publish"] != []
]

if offenders:
    for name in offenders:
        print(f"ERROR: workspace package is publishable: {name}")
    sys.exit(1)

print("OK: All workspace packages have publish = false.")
'
