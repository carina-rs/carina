#!/bin/bash
# Generate aws provider documentation from Smithy models
#
# Usage (from project root):
#   ./carina-provider-aws/scripts/generate-docs.sh
#
# This script generates markdown documentation from Smithy model JSON files.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DOCS_DIR="docs/src/providers/aws"
EXAMPLES_DIR="carina-provider-aws/examples"
mkdir -p "$DOCS_DIR"

# Download models if needed
"$SCRIPT_DIR/download-smithy-models.sh"

echo "Generating aws provider documentation..."
echo "Output directory: $DOCS_DIR"
echo ""

cd "$PROJECT_ROOT"
cargo run --bin smithy-codegen -- \
  --model-dir "$SCRIPT_DIR/../tests/fixtures/smithy" \
  --output-dir "$DOCS_DIR" \
  --format markdown

# Append examples to generated docs
for DOC_FILE in "$DOCS_DIR"/*.md; do
    RESOURCE_NAME=$(basename "$DOC_FILE" .md)
    # Skip non-resource files like index.md
    if [ "$RESOURCE_NAME" = "index" ]; then
        continue
    fi
    EXAMPLE_FILE="$EXAMPLES_DIR/${RESOURCE_NAME}/main.crn"
    if [ -f "$EXAMPLE_FILE" ]; then
        echo "" >> "$DOC_FILE"
        echo "## Example" >> "$DOC_FILE"
        echo "" >> "$DOC_FILE"
        echo '```crn' >> "$DOC_FILE"
        # Strip provider block, leading comments, and leading blank lines
        sed -n '/^provider /,/^}/!p' "$EXAMPLE_FILE" | \
            sed '/^#/d' | \
            sed '/./,$!d' >> "$DOC_FILE"
        echo '```' >> "$DOC_FILE"
    fi
done

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
