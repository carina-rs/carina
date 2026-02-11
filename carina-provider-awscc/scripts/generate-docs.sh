#!/bin/bash
# Generate awscc provider documentation from CloudFormation schemas
#
# Usage (from project root):
#   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-docs.sh
#
# This script generates markdown documentation from CloudFormation resource type schemas.

set -e

DOCS_DIR="docs/src/providers/awscc"
EXAMPLES_DIR="carina-provider-awscc/examples"
mkdir -p "$DOCS_DIR"

# Same resource types as generate-schemas.sh
RESOURCE_TYPES=(
    "AWS::EC2::VPC"
    "AWS::EC2::Subnet"
    "AWS::EC2::InternetGateway"
    "AWS::EC2::RouteTable"
    "AWS::EC2::Route"
    "AWS::EC2::SubnetRouteTableAssociation"
    "AWS::EC2::EIP"
    "AWS::EC2::NatGateway"
    "AWS::EC2::SecurityGroup"
    "AWS::EC2::SecurityGroupIngress"
    "AWS::EC2::SecurityGroupEgress"
    "AWS::EC2::VPCEndpoint"
    "AWS::EC2::VPCGatewayAttachment"
    "AWS::EC2::FlowLog"
    "AWS::EC2::IPAM"
    "AWS::EC2::IPAMPool"
    "AWS::EC2::VPNGateway"
    "AWS::EC2::TransitGateway"
    "AWS::EC2::VPCPeeringConnection"
    "AWS::EC2::EgressOnlyInternetGateway"
    "AWS::EC2::TransitGatewayAttachment"
    "AWS::S3::Bucket"
    "AWS::IAM::Role"
    "AWS::Logs::LogGroup"
)

echo "Generating awscc provider documentation..."
echo "Output directory: $DOCS_DIR"
echo ""

# Build codegen tool first
cargo build -p carina-provider-awscc --bin codegen --quiet 2>/dev/null || true

# Find the built binary
CODEGEN_BIN="target/debug/codegen"
if [ ! -f "$CODEGEN_BIN" ]; then
    echo "ERROR: codegen binary not found at $CODEGEN_BIN"
    echo "Trying to build with cargo..."
    cargo build -p carina-provider-awscc --bin codegen
    if [ ! -f "$CODEGEN_BIN" ]; then
        echo "ERROR: Could not build codegen binary"
        exit 1
    fi
fi

for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    OUTPUT_FILE="$DOCS_DIR/${FULL_RESOURCE}.md"

    echo "Generating $TYPE_NAME -> $OUTPUT_FILE"

    # Generate schema documentation
    aws cloudformation describe-type \
        --type RESOURCE \
        --type-name "$TYPE_NAME" \
        --query 'Schema' \
        --output text 2>/dev/null | \
    "$CODEGEN_BIN" --type-name "$TYPE_NAME" --format markdown > "$OUTPUT_FILE"

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

# Auto-generate SUMMARY.md with service category grouping
echo ""
echo "Generating docs/src/SUMMARY.md"

cat > "docs/src/SUMMARY.md" << 'EOF'
# Summary

[Introduction](introduction.md)

# Providers

- [AWSCC Provider](providers/awscc/index.md)
EOF

# Group resources by service category
PREV_SERVICE=""
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    # Extract service name (e.g., AWS::EC2::VPC -> EC2)
    SERVICE=$(echo "$TYPE_NAME" | awk -F'::' '{print $2}')
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)

    # Write service category header when service changes
    if [ "$SERVICE" != "$PREV_SERVICE" ]; then
        echo "  - [${SERVICE}]()" >> "docs/src/SUMMARY.md"
        PREV_SERVICE="$SERVICE"
    fi

    echo "    - [awscc.${FULL_RESOURCE}](providers/awscc/${FULL_RESOURCE}.md)" >> "docs/src/SUMMARY.md"
done

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
echo ""
echo "To build the book:"
echo "  cargo install mdbook"
echo "  cd docs && mdbook build"
echo ""
echo "To preview:"
echo "  cd docs && mdbook serve"
