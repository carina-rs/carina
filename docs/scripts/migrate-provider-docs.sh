#!/bin/bash
# One-time migration: copy provider docs from mdBook to Starlight layout
# and prepend frontmatter to each file.
set -e

SRC_BASE="docs/src/providers"
DST_BASE="docs/src/content/docs/reference/providers"

migrate_provider() {
    local PROVIDER="$1"
    local SRC_DIR="$SRC_BASE/$PROVIDER"
    local DST_DIR="$DST_BASE/$PROVIDER"

    if [ ! -d "$SRC_DIR" ]; then
        echo "Skipping $PROVIDER (no source directory)"
        return
    fi

    # Copy index.md with frontmatter
    if [ -f "$SRC_DIR/index.md" ]; then
        mkdir -p "$DST_DIR"
        local PROVIDER_UPPER
        PROVIDER_UPPER=$(echo "$PROVIDER" | tr '[:lower:]' '[:upper:]')
        {
            echo "---"
            echo "title: \"${PROVIDER_UPPER} Provider\""
            echo "---"
            echo ""
            # Strip H1 line (Starlight renders frontmatter title as heading)
            sed '1{/^# /d;}' "$SRC_DIR/index.md"
        } > "$DST_DIR/index.md"
        echo "Migrated $DST_DIR/index.md"
    fi

    # Copy resource docs with frontmatter
    for DOC_FILE in "$SRC_DIR"/*/*.md; do
        [ -f "$DOC_FILE" ] || continue

        local SERVICE_DIR
        SERVICE_DIR=$(basename "$(dirname "$DOC_FILE")")
        local RESOURCE_NAME
        RESOURCE_NAME=$(basename "$DOC_FILE" .md)

        # Extract DSL name from first heading (e.g., "# awscc.ec2.vpc" -> "awscc.ec2.vpc")
        local DSL_NAME
        DSL_NAME=$(head -1 "$DOC_FILE" | sed 's/^# *//')

        # Extract service display name from CloudFormation Type line
        local SERVICE_DISPLAY=""
        local CFN_LINE
        CFN_LINE=$(grep "^CloudFormation Type:" "$DOC_FILE" 2>/dev/null | head -1)
        if [ -n "$CFN_LINE" ]; then
            SERVICE_DISPLAY=$(echo "$CFN_LINE" | sed 's/.*`AWS::\([^:]*\)::.*/\1/')
        fi
        if [ -z "$SERVICE_DISPLAY" ]; then
            SERVICE_DISPLAY=$(echo "$SERVICE_DIR" | tr '[:lower:]' '[:upper:]')
        fi

        local PROVIDER_UPPER
        PROVIDER_UPPER=$(echo "$PROVIDER" | tr '[:lower:]' '[:upper:]')

        mkdir -p "$DST_DIR/$SERVICE_DIR"
        local DST_FILE="$DST_DIR/$SERVICE_DIR/$RESOURCE_NAME.md"

        {
            echo "---"
            echo "title: \"$DSL_NAME\""
            echo "description: \"${PROVIDER_UPPER} $SERVICE_DISPLAY ${RESOURCE_NAME} resource reference\""
            echo "---"
            echo ""
            # Strip H1 line (Starlight renders frontmatter title as heading)
            sed '1{/^# /d;}' "$DOC_FILE"
        } > "$DST_FILE"
        echo "Migrated $DST_FILE"
    done
}

migrate_provider "awscc"
migrate_provider "aws"

echo ""
echo "Migration complete. Verify with: cd docs && npm run build"
