#!/bin/bash
# Generate awscc provider schemas from CloudFormation
#
# Usage (from project root):
#   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh
#   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh --refresh-cache
#
# Options:
#   --refresh-cache  Force re-download of all CloudFormation schemas
#
# Downloaded schemas are cached in carina-provider-awscc/cfn-schema-cache/.
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

CACHE_DIR="carina-provider-awscc/cfn-schema-cache"
OUTPUT_DIR="carina-provider-awscc/src/schemas/generated"
mkdir -p "$CACHE_DIR"

# List of resource types to generate
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

echo "Generating awscc provider schemas..."
echo "Output directory: $OUTPUT_DIR"
echo ""

# Build codegen tool first
# Use --quiet to suppress cargo output; build only the binary (not the lib)
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

# Helper: extract service name from CloudFormation type (e.g., AWS::EC2::VPC -> ec2)
service_name() {
    echo "$1" | awk -F'::' '{print tolower($2)}'
}

# Helper: extract resource module name (e.g., AWS::EC2::VPC -> vpc)
resource_module_name() {
    "$CODEGEN_BIN" --type-name "$1" --print-module-name
}

# Remove old flat-structure files (migration from flat to service/resource layout)
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FLAT_NAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    OLD_FILE="$OUTPUT_DIR/${FLAT_NAME}.rs"
    if [ -f "$OLD_FILE" ]; then
        rm -f "$OLD_FILE"
    fi
done

# Collect unique services and create directories
SERVICES=""
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    SVC=$(service_name "$TYPE_NAME")
    # Add to list if not already present
    case " $SERVICES " in
        *" $SVC "*) ;;
        *) SERVICES="$SERVICES $SVC" ;;
    esac
    mkdir -p "$OUTPUT_DIR/$SVC"
done
SERVICES=$(echo "$SERVICES" | tr ' ' '\n' | sort | tr '\n' ' ')

for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    SVC=$(service_name "$TYPE_NAME")
    RESOURCE=$(resource_module_name "$TYPE_NAME")
    OUTPUT_FILE="$OUTPUT_DIR/$SVC/${RESOURCE}.rs"

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

# Generate per-service mod.rs files
for SVC in $SERVICES; do
    SVC_MOD="$OUTPUT_DIR/$SVC/mod.rs"
    echo "Generating $SVC_MOD"

    cat > "$SVC_MOD" << 'EOF'
//! Auto-generated — DO NOT EDIT MANUALLY
//!
//! Regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

// Re-export parent types so resource modules can use `super::` to access them.
pub use super::*;

EOF

    # Add module declarations for resources in this service
    for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
        TYPE_SVC=$(service_name "$TYPE_NAME")
        if [ "$TYPE_SVC" = "$SVC" ]; then
            RESOURCE=$(resource_module_name "$TYPE_NAME")
            echo "pub mod ${RESOURCE};" >> "$SVC_MOD"
        fi
    done
done

# Generate top-level mod.rs
echo ""
echo "Generating $OUTPUT_DIR/mod.rs"

cat > "$OUTPUT_DIR/mod.rs" << 'EOF'
//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

// Re-export all types and validators from awscc_types so that
// generated schema files can use `super::` to access them.
pub use super::awscc_types::*;

EOF

# Add service module declarations
for SVC in $SERVICES; do
    echo "pub mod ${SVC};" >> "$OUTPUT_DIR/mod.rs"
done

# Add configs() function
cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'

/// Returns all generated schema configs
pub fn configs() -> Vec<AwsccSchemaConfig> {
    vec![
EOF

# Add config function calls dynamically
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    SVC=$(service_name "$TYPE_NAME")
    RESOURCE=$(resource_module_name "$TYPE_NAME")
    FULL_NAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    FUNC_NAME="${FULL_NAME}_config"

    echo "        ${SVC}::${RESOURCE}::${FUNC_NAME}()," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    ]
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
    SVC=$(service_name "$TYPE_NAME")
    RESOURCE=$(resource_module_name "$TYPE_NAME")
    echo "        ${SVC}::${RESOURCE}::enum_valid_values()," >> "$OUTPUT_DIR/mod.rs"
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
    SVC=$(service_name "$TYPE_NAME")
    RESOURCE=$(resource_module_name "$TYPE_NAME")
    DSL_NAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-dsl-resource-name)
    cat >> "$OUTPUT_DIR/mod.rs" << INNEREOF
    if resource_type == "${DSL_NAME}" {
        return ${SVC}::${RESOURCE}::enum_alias_reverse(attr_name, value);
    }
INNEREOF
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    None
}
EOF

echo ""
echo "Running cargo fmt..."
cargo fmt -p carina-provider-awscc

echo ""
echo "Done! Generated schemas in $OUTPUT_DIR"
