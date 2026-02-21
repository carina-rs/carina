#!/usr/bin/env bash
# Download AWS Smithy model JSON files for testing.
# Models are cached in tests/fixtures/aws/ and gitignored.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="$SCRIPT_DIR/../tests/fixtures/aws"

mkdir -p "$FIXTURE_DIR"

BASE_URL="https://raw.githubusercontent.com/aws/api-models-aws/main/models"

download() {
    local name="$1"
    local path="$2"
    local dest="$FIXTURE_DIR/${name}.json"
    if [ -f "$dest" ]; then
        echo "Already downloaded: $dest"
    else
        echo "Downloading $name model..."
        curl -fsSL "$BASE_URL/$path" -o "$dest"
        echo "  -> $dest ($(du -h "$dest" | cut -f1))"
    fi
}

download "ec2" "ec2/service/2016-11-15/ec2-2016-11-15.json"
download "s3"  "s3/service/2006-03-01/s3-2006-03-01.json"

echo "Done."
