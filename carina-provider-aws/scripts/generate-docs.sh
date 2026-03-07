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

# Insert examples into generated docs (after description, before Argument Reference)
for DOC_FILE in "$DOCS_DIR"/*.md; do
    RESOURCE_NAME=$(basename "$DOC_FILE" .md)
    # Skip non-resource files like index.md
    if [ "$RESOURCE_NAME" = "index" ]; then
        continue
    fi
    EXAMPLE_FILE="$EXAMPLES_DIR/${RESOURCE_NAME}/main.crn"
    if [ -f "$EXAMPLE_FILE" ]; then
        EXAMPLE_TMPFILE=$(mktemp)
        {
            echo "## Example"
            echo ""
            echo '```crn'
            # Strip provider block, leading comments, and leading blank lines
            sed -n '/^provider /,/^}/!p' "$EXAMPLE_FILE" | \
                sed '/^#/d' | \
                sed '/./,$!d'
            echo '```'
            echo ""
        } > "$EXAMPLE_TMPFILE"
        # Insert the example block before "## Argument Reference"
        MERGED_TMPFILE=$(mktemp)
        while IFS= read -r line || [ -n "$line" ]; do
            if [ "$line" = "## Argument Reference" ]; then
                cat "$EXAMPLE_TMPFILE"
            fi
            printf '%s\n' "$line"
        done < "$DOC_FILE" > "$MERGED_TMPFILE"
        mv "$MERGED_TMPFILE" "$DOC_FILE"
        rm -f "$EXAMPLE_TMPFILE"
    fi
done

# Generate SUMMARY.md (shared across all providers)
echo ""
"$PROJECT_ROOT/docs/scripts/generate-summary.sh"

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
