#!/bin/bash
# Generate awscc provider schemas from CloudFormation
#
# Usage:
#   aws-vault exec <profile> -- ./scripts/generate-awscc-schemas.sh
#
# This script generates Rust schema code from CloudFormation resource type schemas.

set -e

OUTPUT_DIR="carina-provider-awscc/src/schemas/generated"
mkdir -p "$OUTPUT_DIR"

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
    "AWS::EC2::VPCEndpoint"
    "AWS::EC2::VPCGatewayAttachment"
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

for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    # Convert AWS::EC2::VPC -> vpc.rs
    FILENAME=$(echo "$TYPE_NAME" | sed 's/AWS::EC2:://' | tr '[:upper:]' '[:lower:]')
    # Handle special cases
    FILENAME=$(echo "$FILENAME" | sed 's/subnetroutetableassociation/route_table_association/')
    FILENAME=$(echo "$FILENAME" | sed 's/vpcgatewayattachment/vpc_gateway_attachment/')
    FILENAME=$(echo "$FILENAME" | sed 's/vpcendpoint/vpc_endpoint/')
    FILENAME=$(echo "$FILENAME" | sed 's/natgateway/nat_gateway/')
    FILENAME=$(echo "$FILENAME" | sed 's/internetgateway/internet_gateway/')
    FILENAME=$(echo "$FILENAME" | sed 's/routetable/route_table/')
    FILENAME=$(echo "$FILENAME" | sed 's/securitygroupingress/security_group_ingress/')
    FILENAME=$(echo "$FILENAME" | sed 's/securitygroup/security_group/')

    OUTPUT_FILE="$OUTPUT_DIR/${FILENAME}.rs"

    echo "Generating $TYPE_NAME -> $OUTPUT_FILE"

    aws cloudformation describe-type \
        --type RESOURCE \
        --type-name "$TYPE_NAME" \
        --query 'Schema' \
        --output text 2>/dev/null | \
    "$CODEGEN_BIN" --type-name "$TYPE_NAME" > "$OUTPUT_FILE"

    if [ $? -ne 0 ]; then
        echo "  ERROR: Failed to generate $TYPE_NAME"
        rm -f "$OUTPUT_FILE"
    fi
done

# Generate mod.rs
echo ""
echo "Generating $OUTPUT_DIR/mod.rs"

cat > "$OUTPUT_DIR/mod.rs" << 'EOF'
//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./scripts/generate-awscc-schemas.sh

use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema};

/// AWS Cloud Control schema configuration
///
/// Combines the generated ResourceSchema with AWS-specific metadata
/// that was previously in ResourceConfig.
pub struct AwsccSchemaConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Whether this resource type uses tags
    pub has_tags: bool,
    /// The resource schema with attribute definitions
    pub schema: ResourceSchema,
}

/// Tags type for AWS resources (Terraform-style map)
pub fn tags_type() -> AttributeType {
    AttributeType::Map(Box::new(AttributeType::String))
}

/// Normalize a namespaced enum value to its base value.
/// Handles formats like:
/// - "value" -> "value"
/// - "TypeName.value" -> "value"
/// - "awscc.resource.TypeName.value" -> "value"
pub fn normalize_namespaced_enum(s: &str) -> String {
    if s.contains('.') {
        let parts: Vec<&str> = s.split('.').collect();
        parts.last().map(|s| s.to_string()).unwrap_or_default()
    } else {
        s.to_string()
    }
}

/// Validate a namespaced enum value.
/// Returns Ok(()) if valid, Err with message if invalid.
pub fn validate_namespaced_enum(
    value: &Value,
    type_name: &str,
    namespace: &str,
    valid_values: &[&str],
) -> Result<(), String> {
    if let Value::String(s) = value {
        // Validate namespace format if it contains dots
        if s.contains('.') {
            let parts: Vec<&str> = s.split('.').collect();
            match parts.len() {
                // 2-part: TypeName.value
                2 => {
                    if parts[0] != type_name {
                        return Err(format!(
                            "Invalid format '{}', expected {}.value",
                            s, type_name
                        ));
                    }
                }
                // 4-part: awscc.resource.TypeName.value
                4 => {
                    let expected_namespace: Vec<&str> = namespace.split('.').collect();
                    if expected_namespace.len() != 2
                        || parts[0] != expected_namespace[0]
                        || parts[1] != expected_namespace[1]
                        || parts[2] != type_name
                    {
                        return Err(format!(
                            "Invalid format '{}', expected {}.{}.value",
                            s, namespace, type_name
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "Invalid format '{}', expected one of: value, {}.value, or {}.{}.value",
                        s, type_name, namespace, type_name
                    ));
                }
            }
        }

        let normalized = normalize_namespaced_enum(s);
        if valid_values.contains(&normalized.as_str()) {
            Ok(())
        } else {
            Err(format!(
                "Invalid value '{}', expected one of: {}",
                s,
                valid_values.join(", ")
            ))
        }
    } else {
        Err("Expected string".to_string())
    }
}

EOF

# Add module declarations
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    MODNAME=$(echo "$TYPE_NAME" | sed 's/AWS::EC2:://' | tr '[:upper:]' '[:lower:]')
    MODNAME=$(echo "$MODNAME" | sed 's/subnetroutetableassociation/route_table_association/')
    MODNAME=$(echo "$MODNAME" | sed 's/vpcgatewayattachment/vpc_gateway_attachment/')
    MODNAME=$(echo "$MODNAME" | sed 's/vpcendpoint/vpc_endpoint/')
    MODNAME=$(echo "$MODNAME" | sed 's/natgateway/nat_gateway/')
    MODNAME=$(echo "$MODNAME" | sed 's/internetgateway/internet_gateway/')
    MODNAME=$(echo "$MODNAME" | sed 's/routetable/route_table/')
    MODNAME=$(echo "$MODNAME" | sed 's/securitygroupingress/security_group_ingress/')
    MODNAME=$(echo "$MODNAME" | sed 's/securitygroup/security_group/')

    echo "pub mod ${MODNAME};" >> "$OUTPUT_DIR/mod.rs"
done

# Add configs() function
cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'

/// Returns all generated schema configs
pub fn configs() -> Vec<AwsccSchemaConfig> {
    vec![
EOF

# Add config function calls dynamically
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    # AWS::EC2::VPC -> ec2, vpc
    SERVICE=$(echo "$TYPE_NAME" | sed 's/AWS::\([^:]*\)::.*/\1/' | tr '[:upper:]' '[:lower:]')
    RESOURCE=$(echo "$TYPE_NAME" | sed 's/AWS::[^:]*:://' | tr '[:upper:]' '[:lower:]')

    # Convert to snake_case
    RESOURCE=$(echo "$RESOURCE" | sed 's/\([A-Z]\)/_\L\1/g' | sed 's/^_//')
    # Handle special naming
    RESOURCE=$(echo "$RESOURCE" | sed 's/subnetroutetableassociation/subnet_route_table_association/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/vpcgatewayattachment/vpc_gateway_attachment/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/vpcendpoint/vpc_endpoint/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/natgateway/nat_gateway/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/internetgateway/internet_gateway/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/routetable/route_table/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/securitygroupingress/security_group_ingress/')
    RESOURCE=$(echo "$RESOURCE" | sed 's/securitygroup/security_group/')

    # Module name (same as MODNAME above)
    MODNAME=$(echo "$TYPE_NAME" | sed 's/AWS::EC2:://' | tr '[:upper:]' '[:lower:]')
    MODNAME=$(echo "$MODNAME" | sed 's/subnetroutetableassociation/route_table_association/')
    MODNAME=$(echo "$MODNAME" | sed 's/vpcgatewayattachment/vpc_gateway_attachment/')
    MODNAME=$(echo "$MODNAME" | sed 's/vpcendpoint/vpc_endpoint/')
    MODNAME=$(echo "$MODNAME" | sed 's/natgateway/nat_gateway/')
    MODNAME=$(echo "$MODNAME" | sed 's/internetgateway/internet_gateway/')
    MODNAME=$(echo "$MODNAME" | sed 's/routetable/route_table/')
    MODNAME=$(echo "$MODNAME" | sed 's/securitygroupingress/security_group_ingress/')
    MODNAME=$(echo "$MODNAME" | sed 's/securitygroup/security_group/')

    # Function name: service_resource_config (e.g., ec2_vpc_config)
    FUNC_NAME="${SERVICE}_${RESOURCE}_config"

    echo "        ${MODNAME}::${FUNC_NAME}()," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    ]
}

/// Returns all generated schemas (for backward compatibility)
pub fn schemas() -> Vec<ResourceSchema> {
    configs().into_iter().map(|c| c.schema).collect()
}
EOF

echo ""
echo "Done! Generated schemas in $OUTPUT_DIR"
echo ""
echo "To use the generated schemas, update carina-provider-awscc/src/schemas/mod.rs"
