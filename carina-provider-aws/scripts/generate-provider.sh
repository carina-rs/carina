#!/usr/bin/env bash
# Generate aws provider boilerplate from ResourceDef metadata
#
# Usage (from project root):
#   ./carina-provider-aws/scripts/generate-provider.sh
#
# This script generates simple delete methods and no-op update methods
# from ResourceDef metadata.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Build and run codegen
cd "$PROJECT_ROOT"
cargo run -p carina-codegen-aws --bin smithy-codegen -- \
  --model-dir "$SCRIPT_DIR/../tests/fixtures/smithy" \
  --output-dir "$SCRIPT_DIR/../src" \
  --format provider

# Format
cargo fmt -p carina-provider-aws

echo ""
echo "Done! Generated carina-provider-aws/src/provider_generated.rs"
