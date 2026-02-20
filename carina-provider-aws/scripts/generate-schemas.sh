#!/bin/bash
# Generate aws provider schemas from CloudFormation
#
# Usage (from project root):
#   aws-vault exec <profile> -- ./carina-provider-aws/scripts/generate-schemas.sh
#   aws-vault exec <profile> -- ./carina-provider-aws/scripts/generate-schemas.sh --refresh-cache
#
# Options:
#   --refresh-cache  Force re-download of all CloudFormation schemas
#
# Downloaded schemas are cached in cfn-schema-cache/ (shared with awscc provider).
# Subsequent runs use cached schemas unless --refresh-cache is specified.
#
# This script generates Rust schema code from CloudFormation resource type schemas.

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
OUTPUT_DIR="carina-provider-aws/src/schemas/generated"
mkdir -p "$CACHE_DIR"
mkdir -p "$OUTPUT_DIR"

# List of resource types to generate
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

echo "Generating aws provider schemas..."
echo "Output directory: $OUTPUT_DIR"
echo ""

# Build codegen tool first
# Use --quiet to suppress cargo output; build only the binary (not the lib)
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
    # Use codegen to compute the full resource name (e.g., ec2_vpc)
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    OUTPUT_FILE="$OUTPUT_DIR/${FULL_RESOURCE}.rs"

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

    "$CODEGEN_BIN" --type-name "$TYPE_NAME" < "$CACHE_FILE" > "$OUTPUT_FILE"

    if [ $? -ne 0 ]; then
        echo "  ERROR: Failed to generate $TYPE_NAME"
        rm -f "$OUTPUT_FILE"
    fi
done

# Generate mod.rs
echo ""
echo "Generating $OUTPUT_DIR/mod.rs"

cat > "$OUTPUT_DIR/mod.rs" << 'EOF'
//! Auto-generated AWS provider resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-aws/scripts/generate-schemas.sh

use carina_core::schema::ResourceSchema;

// Re-export all types and validators from types so that
// generated schema files can use `super::` to access them.
pub use super::types::*;

EOF

# Add module declarations
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    echo "pub mod ${FULL_RESOURCE};" >> "$OUTPUT_DIR/mod.rs"
done

# Add configs() function
cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'

/// Returns all generated schema configs
pub fn configs() -> Vec<AwsSchemaConfig> {
    vec![
EOF

# Add config function calls dynamically
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    FUNC_NAME="${FULL_RESOURCE}_config"

    echo "        ${FULL_RESOURCE}::${FUNC_NAME}()," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    ]
}

/// Returns all generated schemas (for backward compatibility)
pub fn schemas() -> Vec<ResourceSchema> {
    configs().into_iter().map(|c| c.schema).collect()
}

/// Get valid enum values for a given resource type and attribute name.
/// Used during read-back to normalize AWS-returned values to canonical DSL form.
///
/// Auto-generated from schema enum constants.
#[allow(clippy::type_complexity)]
pub fn get_enum_valid_values(resource_type: &str, attr_name: &str) -> Option<&'static [&'static str]> {
    let modules: &[(&str, &[(&str, &[&str])])] = &[
EOF

# Add enum_valid_values() calls dynamically
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    echo "        ${FULL_RESOURCE}::enum_valid_values()," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    ];
    for (rt, attrs) in modules {
        if *rt == resource_type {
            for (attr, values) in *attrs {
                if *attr == attr_name {
                    return Some(values);
                }
            }
            return None;
        }
    }
    None
}

/// Maps DSL alias values back to canonical AWS values.
/// Dispatches to per-module enum_alias_reverse() functions.
pub fn get_enum_alias_reverse(resource_type: &str, attr_name: &str, value: &str) -> Option<&'static str> {
EOF

# Add enum_alias_reverse() dispatches dynamically
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    cat >> "$OUTPUT_DIR/mod.rs" << INNEREOF
    if resource_type == "${FULL_RESOURCE}" {
        return ${FULL_RESOURCE}::enum_alias_reverse(attr_name, value);
    }
INNEREOF
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    None
}
EOF

echo ""
echo "Running cargo fmt..."
cargo fmt -p carina-provider-aws

echo ""
echo "Done! Generated schemas in $OUTPUT_DIR"
