#!/usr/bin/env bash
# Classify supported release tags before building or publishing artifacts.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo '::error::Expected one release tag: vX.Y.Z or vX.Y.Z-rc.N' >&2
    exit 1
fi

if [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo stable
elif [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$ ]]; then
    echo rc
else
    printf "::error::Unsupported release tag '%s'; expected vX.Y.Z or vX.Y.Z-rc.N\n" "$1" >&2
    exit 1
fi
