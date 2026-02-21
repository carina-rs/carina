#!/usr/bin/env bash
# Generate aws provider schemas from Smithy models
#
# Usage (from project root):
#   ./carina-provider-aws/scripts/generate-schemas-smithy.sh
#
# This script generates Rust schema code from AWS Smithy model JSON files.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Download models if needed
"$SCRIPT_DIR/download-smithy-models.sh"

# Build and run codegen
cd "$PROJECT_ROOT"
cargo run --bin smithy-codegen -- \
  --model-dir "$SCRIPT_DIR/../tests/fixtures/smithy" \
  --output-dir "$SCRIPT_DIR/../src/schemas/generated"

# Format
cargo fmt -p carina-provider-aws

echo ""
echo "Done! Generated schemas in carina-provider-aws/src/schemas/generated/"
