#!/bin/bash
# Generate docs/src/SUMMARY.md from existing provider documentation files
#
# Usage (from project root):
#   ./docs/scripts/generate-summary.sh
#
# This script scans docs/src/providers/{awscc,aws}/ for generated markdown files
# and assembles a complete SUMMARY.md with service category grouping.
# It extracts DSL names from headings and service names from CloudFormation Type lines.

set -e

SUMMARY_FILE="docs/src/SUMMARY.md"

echo "Generating $SUMMARY_FILE"

# Write header
cat > "$SUMMARY_FILE" << 'EOF'
# Summary

[Introduction](introduction.md)

# Providers

EOF

# Generate SUMMARY entries for a single provider
#   $1 = provider directory name (e.g., "awscc", "aws")
#   $2 = provider display label (e.g., "AWSCC Provider", "AWS Provider")
generate_provider_section() {
    local PROVIDER="$1"
    local LABEL="$2"
    local DOCS_DIR="docs/src/providers/$PROVIDER"

    if [ ! -d "$DOCS_DIR" ]; then
        return
    fi

    # Check if there are any resource doc files (exclude index.md)
    local HAS_FILES=false
    for f in "$DOCS_DIR"/*.md; do
        [ "$(basename "$f")" = "index.md" ] && continue
        HAS_FILES=true
        break
    done

    if [ "$HAS_FILES" = false ]; then
        return
    fi

    echo "- [${LABEL}](providers/${PROVIDER}/index.md)" >> "$SUMMARY_FILE"

    local PREV_SERVICE=""
    for DOC_FILE in "$DOCS_DIR"/*.md; do
        local BASENAME
        BASENAME=$(basename "$DOC_FILE" .md)
        [ "$BASENAME" = "index" ] && continue

        # Extract service display name from CloudFormation Type line
        # e.g., "CloudFormation Type: `AWS::EC2::VPC`" -> "EC2"
        local SERVICE_DISPLAY=""
        local CFN_LINE
        CFN_LINE=$(grep "^CloudFormation Type:" "$DOC_FILE" 2>/dev/null | head -1)
        if [ -n "$CFN_LINE" ]; then
            SERVICE_DISPLAY=$(echo "$CFN_LINE" | sed 's/.*`AWS::\([^:]*\)::.*/\1/')
        fi

        if [ -z "$SERVICE_DISPLAY" ]; then
            # Fallback: uppercase first segment of filename
            SERVICE_DISPLAY=$(echo "$BASENAME" | awk -F'_' '{print toupper($1)}')
        fi

        # Extract DSL name from first heading
        # e.g., "# awscc.ec2.vpc" -> "awscc.ec2.vpc"
        local DSL_NAME
        DSL_NAME=$(head -1 "$DOC_FILE" | sed 's/^# *//')

        # Group by service
        if [ "$SERVICE_DISPLAY" != "$PREV_SERVICE" ]; then
            echo "  - [${SERVICE_DISPLAY}]()" >> "$SUMMARY_FILE"
            PREV_SERVICE="$SERVICE_DISPLAY"
        fi

        echo "    - [${DSL_NAME}](providers/${PROVIDER}/${BASENAME}.md)" >> "$SUMMARY_FILE"
    done
}

generate_provider_section "awscc" "AWSCC Provider"
generate_provider_section "aws" "AWS Provider"

echo "Done: $SUMMARY_FILE"
