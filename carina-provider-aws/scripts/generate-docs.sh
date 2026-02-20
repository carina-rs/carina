#!/bin/bash
# Generate aws provider documentation from CloudFormation schemas
#
# Usage (from project root):
#   aws-vault exec <profile> -- ./carina-provider-aws/scripts/generate-docs.sh
#   aws-vault exec <profile> -- ./carina-provider-aws/scripts/generate-docs.sh --refresh-cache
#
# Options:
#   --refresh-cache  Force re-download of all CloudFormation schemas
#
# Downloaded schemas are cached in cfn-schema-cache/ (shared with awscc provider).
# Subsequent runs use cached schemas unless --refresh-cache is specified.
#
# This script generates markdown documentation from CloudFormation resource type schemas.

set -e

# Parse flags
REFRESH_CACHE=false
for arg in "$@"; do
    case "$arg" in
        --refresh-cache) REFRESH_CACHE=true ;;
    esac
done

# Use AWSCC's cache directory (shared between providers)
CACHE_DIR="carina-provider-awscc/cfn-schema-cache"
DOCS_DIR="docs/src/providers/aws"
EXAMPLES_DIR="carina-provider-aws/examples"
mkdir -p "$CACHE_DIR"
mkdir -p "$DOCS_DIR"

# Same resource types as generate-schemas.sh
RESOURCE_TYPES=(
    "AWS::EC2::VPC"
    "AWS::EC2::Subnet"
    "AWS::EC2::InternetGateway"
    "AWS::EC2::RouteTable"
    "AWS::EC2::Route"
    "AWS::EC2::SecurityGroup"
    "AWS::EC2::SecurityGroupIngress"
    "AWS::EC2::SecurityGroupEgress"
    "AWS::S3::Bucket"
)

echo "Generating aws provider documentation..."
echo "Output directory: $DOCS_DIR"
echo ""

# Build codegen tool first
cargo build -p carina-provider-aws --bin aws-codegen --quiet 2>/dev/null || true

# Find the built binary
CODEGEN_BIN="target/debug/aws-codegen"
if [ ! -f "$CODEGEN_BIN" ]; then
    echo "ERROR: aws-codegen binary not found at $CODEGEN_BIN"
    echo "Trying to build with cargo..."
    cargo build -p carina-provider-aws --bin aws-codegen
    if [ ! -f "$CODEGEN_BIN" ]; then
        echo "ERROR: Could not build aws-codegen binary"
        exit 1
    fi
fi

for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    OUTPUT_FILE="$DOCS_DIR/${FULL_RESOURCE}.md"

    echo "Generating $TYPE_NAME -> $OUTPUT_FILE"

    # Cache CloudFormation schema to avoid redundant API calls
    CACHE_FILE="$CACHE_DIR/${TYPE_NAME//::/__}.json"
    if [ "$REFRESH_CACHE" = true ] || [ ! -f "$CACHE_FILE" ]; then
        aws cloudformation describe-type \
            --type RESOURCE \
            --type-name "$TYPE_NAME" \
            --query 'Schema' \
            --output text 2>/dev/null > "$CACHE_FILE"
    else
        echo "  Using cached schema"
    fi

    # Generate schema documentation
    "$CODEGEN_BIN" --type-name "$TYPE_NAME" --format markdown < "$CACHE_FILE" > "$OUTPUT_FILE"

    if [ $? -ne 0 ]; then
        echo "  ERROR: Failed to generate $TYPE_NAME"
        rm -f "$OUTPUT_FILE"
        continue
    fi

    # Append example from hand-written example file
    EXAMPLE_FILE="$EXAMPLES_DIR/${FULL_RESOURCE}/main.crn"
    if [ -f "$EXAMPLE_FILE" ]; then
        echo "" >> "$OUTPUT_FILE"
        echo "## Example" >> "$OUTPUT_FILE"
        echo "" >> "$OUTPUT_FILE"
        echo '```crn' >> "$OUTPUT_FILE"
        # Strip provider block, leading comments, and leading blank lines
        sed -n '/^provider /,/^}/!p' "$EXAMPLE_FILE" | \
            sed '/^#/d' | \
            sed '/./,$!d' >> "$OUTPUT_FILE"
        echo '```' >> "$OUTPUT_FILE"
    fi
done

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
