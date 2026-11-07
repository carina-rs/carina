#!/usr/bin/env bash
# Keep fixture bypasses out of every workspace member's non-dev dependency graph.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
manifest_path=${1:-"$repo_root"}
if [[ -d "$manifest_path" ]]; then
    manifest_path="$manifest_path/Cargo.toml"
fi

tree_file=$(mktemp)
trap 'rm -f "$tree_file"' EXIT

# With resolver 2, excluding dev edges also excludes their feature activations.
# Include all targets and resolved feature lists: checking only feature edges
# would miss a workspace root that enables its own test-utils through default.
cargo tree --manifest-path "$manifest_path" --workspace \
    --edges normal,build,features --target all --color never \
    --prefix depth --format '{p} features=[{f}]' > "$tree_file"

python3 - "$tree_file" <<'PY'
from pathlib import Path
import re
import sys

path = []
offenders = set()
for line in Path(sys.argv[1]).read_text().splitlines():
    entry = re.fullmatch(r"(\d+)(.*)", line)
    if entry is None:
        # Cargo also prints section headings such as [build-dependencies].
        continue
    depth = int(entry[1])
    node = entry[2].removesuffix(" (*)")
    package, separator, features = node.rpartition(" features=[")
    path[depth:] = [package if separator else node]
    if not separator or "test-utils" not in features.removesuffix("]").split(","):
        continue
    if package in offenders:
        continue
    offenders.add(package)
    print('ERROR: feature "test-utils" is enabled in the non-dev dependency graph:', file=sys.stderr)
    print("  " + "\n  -> ".join(path), file=sys.stderr)
    print('  -> feature "test-utils"', file=sys.stderr)

if offenders:
    print("Enable test-utils only from dev-dependencies, never normal/build dependencies or default features.", file=sys.stderr)
    sys.exit(1)

print("OK: No test-utils feature is enabled in the resolved non-dev workspace dependency graph.")
PY
