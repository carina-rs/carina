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

use std::collections::HashMap;
use std::sync::LazyLock;

// Re-export all types and validators from awscc_types so that
// generated schema files can use `super::` to access them.
pub use super::awscc_types::*;

EOF

# Add service module declarations
for SVC in $SERVICES; do
    echo "pub mod ${SVC};" >> "$OUTPUT_DIR/mod.rs"
done

# --- SCHEMA_CONFIGS LazyLock ---
cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'

/// Cached schema configs, initialized once on first access.
static SCHEMA_CONFIGS: LazyLock<Vec<AwsccSchemaConfig>> = LazyLock::new(build_configs);

/// Index from resource_type_name (e.g., "ec2.vpc") to position in SCHEMA_CONFIGS.
static SCHEMA_CONFIG_INDEX: LazyLock<HashMap<&'static str, usize>> = LazyLock::new(|| {
    SCHEMA_CONFIGS
        .iter()
        .enumerate()
        .map(|(i, c)| (c.resource_type_name, i))
        .collect()
});

/// Cached enum valid values: resource_type -> (attr_name -> valid values slice).
static ENUM_VALID_VALUES: LazyLock<
    HashMap<&'static str, HashMap<&'static str, &'static [&'static str]>>,
> = LazyLock::new(|| {
    #[allow(clippy::type_complexity)]
    let modules: &[(&str, &[(&str, &[&str])])] = &[
EOF

# Add enum_valid_values() calls for ENUM_VALID_VALUES
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    SVC=$(service_name "$TYPE_NAME")
    RESOURCE=$(resource_module_name "$TYPE_NAME")
    echo "        ${SVC}::${RESOURCE}::enum_valid_values()," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    ];
    let mut map: HashMap<&str, HashMap<&str, &[&str]>> = HashMap::new();
    for (rt, attrs) in modules {
        let attr_map = map.entry(rt).or_default();
        for (attr, values) in *attrs {
            attr_map.insert(attr, values);
        }
    }
    map
});

/// Function signature for enum alias reverse lookups.
type EnumAliasReverseFn = fn(&str, &str) -> Option<&'static str>;

/// Enum alias reverse dispatch table: resource_type -> dispatch function.
static ENUM_ALIAS_DISPATCH: LazyLock<HashMap<&'static str, EnumAliasReverseFn>> =
    LazyLock::new(|| {
        let entries: Vec<(&str, EnumAliasReverseFn)> = vec![
EOF

# Add enum_alias_reverse dispatch entries
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    SVC=$(service_name "$TYPE_NAME")
    RESOURCE=$(resource_module_name "$TYPE_NAME")
    DSL_NAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-dsl-resource-name)
    echo "            (\"${DSL_NAME}\", ${SVC}::${RESOURCE}::enum_alias_reverse)," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
        ];
        entries.into_iter().collect()
    });

/// Build all schema configs (called once by LazyLock).
fn build_configs() -> Vec<AwsccSchemaConfig> {
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

/// Returns a reference to the cached schema configs slice.
pub fn configs() -> &'static [AwsccSchemaConfig] {
    &SCHEMA_CONFIGS
}

/// Look up a schema config by resource_type_name (e.g., "ec2.vpc"). O(1).
pub fn get_config_by_type(resource_type: &str) -> Option<&'static AwsccSchemaConfig> {
    SCHEMA_CONFIG_INDEX
        .get(resource_type)
        .map(|&i| &SCHEMA_CONFIGS[i])
}

/// Get valid enum values for a given resource type and attribute name. O(1).
/// Used during read-back to normalize AWS-returned values to canonical DSL form.
///
/// Auto-generated from schema enum constants.
pub fn get_enum_valid_values(resource_type: &str, attr_name: &str) -> Option<&'static [&'static str]> {
    ENUM_VALID_VALUES
        .get(resource_type)
        .and_then(|attrs| attrs.get(attr_name))
        .copied()
}

/// Maps DSL alias values back to canonical AWS values. O(1) dispatch.
/// Dispatches to per-module enum_alias_reverse() functions.
pub fn get_enum_alias_reverse(resource_type: &str, attr_name: &str, value: &str) -> Option<&'static str> {
    ENUM_ALIAS_DISPATCH
        .get(resource_type)
        .and_then(|f| f(attr_name, value))
}
EOF

echo ""
echo "Running cargo fmt..."
cargo fmt -p carina-provider-awscc

echo ""
echo "Done! Generated schemas in $OUTPUT_DIR"
