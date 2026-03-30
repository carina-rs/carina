#!/bin/bash
# Generate awscc provider documentation from CloudFormation schemas
#
# Usage (from project root):
#   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-docs.sh
#   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-docs.sh --refresh-cache
#
# Options:
#   --refresh-cache  Force re-download of all CloudFormation schemas
#
# Downloaded schemas are cached in carina-provider-awscc/cfn-schema-cache/.
# Subsequent runs use cached schemas unless --refresh-cache is specified.
#
# This script generates markdown documentation from CloudFormation resource type schemas.

set -e

# Temp file variables (initialized for trap safety)
EXAMPLE_TMPFILE=""
MERGED_TMPFILE=""

# Cleanup temp files on exit (normal or error)
cleanup() {
    [ -n "$EXAMPLE_TMPFILE" ] && rm -f "$EXAMPLE_TMPFILE"
    [ -n "$MERGED_TMPFILE" ] && rm -f "$MERGED_TMPFILE"
}
trap cleanup EXIT

# Parse flags
REFRESH_CACHE=false
for arg in "$@"; do
    case "$arg" in
        --refresh-cache) REFRESH_CACHE=true ;;
    esac
done

CACHE_DIR="carina-provider-awscc/cfn-schema-cache"
DOCS_DIR="docs/src/content/docs/reference/providers/awscc"
EXAMPLES_DIR="carina-provider-awscc/examples"
mkdir -p "$CACHE_DIR"
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
    DSL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-dsl-resource-name)
    SERVICE="${DSL_RESOURCE%%.*}"
    RESOURCE="${DSL_RESOURCE#*.}"
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    mkdir -p "$DOCS_DIR/$SERVICE"
    OUTPUT_FILE="$DOCS_DIR/${SERVICE}/${RESOURCE}.md"

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

    # Prepend Starlight frontmatter and strip H1 (Starlight renders title as heading)
    DSL_NAME=$(head -1 "$OUTPUT_FILE" | sed 's/^# *//')
    FRONTMATTER_TMPFILE=$(mktemp)
    {
        echo "---"
        echo "title: \"$DSL_NAME\""
        echo "description: \"AWSCC $SERVICE $RESOURCE resource reference\""
        echo "---"
        echo ""
        sed '1{/^# /d;}' "$OUTPUT_FILE"
    } > "$FRONTMATTER_TMPFILE"
    mv "$FRONTMATTER_TMPFILE" "$OUTPUT_FILE"

    # Insert example from hand-written example file (after description, before Argument Reference)
    EXAMPLE_FILE="$EXAMPLES_DIR/${FULL_RESOURCE}/main.crn"
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
        done < "$OUTPUT_FILE" > "$MERGED_TMPFILE"
        mv "$MERGED_TMPFILE" "$OUTPUT_FILE"
        rm -f "$EXAMPLE_TMPFILE"
    fi
done

# Auto-generate index.md with categorized resource listing
echo "Generating $DOCS_DIR/index.md"

cat > "$DOCS_DIR/index.md" << 'EOF'
---
title: "AWSCC Provider"
description: "AWSCC provider resource reference"
---

The `awscc` provider manages AWS resources through the [AWS Cloud Control API](https://docs.aws.amazon.com/cloudcontrolapi/latest/userguide/what-is-cloudcontrolapi.html).

## Configuration

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}
```

## Usage

Resources are defined using the `awscc.<service>.<resource_type>` syntax:

```crn
let vpc = awscc.ec2.vpc {
  name       = "my-vpc"
  cidr_block = "10.0.0.0/16"
  tags = {
    Environment = "production"
  }
}
```

Named resources (using `let`) can be referenced by other resources:

```crn
let subnet = awscc.ec2.subnet {
  name              = "my-subnet"
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = "ap-northeast-1a"
}
```

## Enum Values

Some attributes accept enum values. These can be specified in three formats:

- **Bare value**: `instance_tenancy = default`
- **TypeName.value**: `instance_tenancy = InstanceTenancy.default`
- **Full namespace**: `instance_tenancy = awscc.ec2.vpc.InstanceTenancy.default`
EOF

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
