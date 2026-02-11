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

for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    # Use codegen to compute the module name (e.g., security_group_egress)
    MODNAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-module-name)
    OUTPUT_FILE="$OUTPUT_DIR/${MODNAME}.rs"

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
//! Auto-generated AWS Cloud Control resource schemas
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   aws-vault exec <profile> -- ./carina-provider-awscc/scripts/generate-schemas.sh

use carina_core::resource::Value;
use carina_core::schema::{AttributeType, ResourceSchema};

/// AWS Cloud Control schema configuration
///
/// Combines the generated ResourceSchema with AWS-specific metadata
/// that was previously in ResourceConfig.
pub struct AwsccSchemaConfig {
    /// AWS CloudFormation type name (e.g., "AWS::EC2::VPC")
    pub aws_type_name: &'static str,
    /// Resource type name used in DSL (e.g., "ec2_vpc")
    pub resource_type_name: &'static str,
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
        // Accept both underscore (DSL identifier) and hyphen (AWS value) forms
        // e.g., "cloud_watch_logs" matches "cloud-watch-logs"
        let hyphenated = normalized.replace('_', "-");
        if valid_values.contains(&normalized.as_str())
            || valid_values.contains(&hyphenated.as_str())
        {
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

/// IPAM Pool ID type (e.g., "ipam-pool-0123456789abcdef0")
/// Validates format: ipam-pool-{hex} where hex is 8+ hex digits
pub fn ipam_pool_id() -> AttributeType {
    AttributeType::Custom {
        name: "IpamPoolId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_ipam_pool_id(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

pub fn validate_ipam_pool_id(id: &str) -> Result<(), String> {
    let Some(hex_part) = id.strip_prefix("ipam-pool-") else {
        return Err(format!(
            "Invalid IPAM Pool ID '{}': expected format 'ipam-pool-{{hex}}'", id
        ));
    };
    if hex_part.len() < 8 {
        return Err(format!(
            "Invalid IPAM Pool ID '{}': hex part must be at least 8 characters", id
        ));
    }
    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid IPAM Pool ID '{}': hex part must contain only hex digits", id
        ));
    }
    Ok(())
}

/// ARN type (e.g., "arn:aws:s3:::my-bucket")
pub fn arn() -> AttributeType {
    AttributeType::Custom {
        name: "Arn".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_arn(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

pub fn validate_arn(arn: &str) -> Result<(), String> {
    if !arn.starts_with("arn:") {
        return Err(format!("Invalid ARN '{}': must start with 'arn:'", arn));
    }
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() < 6 {
        return Err(format!(
            "Invalid ARN '{}': must have at least 6 colon-separated parts (arn:partition:service:region:account:resource)",
            arn
        ));
    }
    Ok(())
}

/// AWS resource ID type (e.g., "vpc-1a2b3c4d", "subnet-0123456789abcdef0")
/// Validates format: {prefix}-{hex} where hex is 8+ hex digits
pub fn aws_resource_id() -> AttributeType {
    AttributeType::Custom {
        name: "AwsResourceId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_aws_resource_id(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

pub fn validate_aws_resource_id(id: &str) -> Result<(), String> {
    let Some(dash_pos) = id.find('-') else {
        return Err(format!(
            "Invalid resource ID '{}': expected format 'prefix-hexdigits'",
            id
        ));
    };

    let prefix = &id[..dash_pos];
    let hex_part = &id[dash_pos + 1..];

    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(format!(
            "Invalid resource ID '{}': prefix must be lowercase alphanumeric",
            id
        ));
    }

    if hex_part.len() < 8 {
        return Err(format!(
            "Invalid resource ID '{}': ID part must be at least 8 characters after prefix",
            id
        ));
    }

    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid resource ID '{}': ID part must contain only hex digits",
            id
        ));
    }

    Ok(())
}

/// Validate a resource ID with a specific prefix
fn validate_prefixed_resource_id(id: &str, expected_prefix: &str) -> Result<(), String> {
    let expected_format = format!("{}-xxxxxxxx", expected_prefix);
    if !id.starts_with(&format!("{}-", expected_prefix)) {
        return Err(format!(
            "Invalid resource ID '{}': expected format '{}'",
            id, expected_format
        ));
    }
    // Reuse existing validation for the rest
    validate_aws_resource_id(id)
}

/// VPC ID type (e.g., "vpc-1a2b3c4d")
pub fn vpc_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vpc")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Subnet ID type (e.g., "subnet-0123456789abcdef0")
pub fn subnet_id() -> AttributeType {
    AttributeType::Custom {
        name: "SubnetId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "subnet")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Security Group ID type (e.g., "sg-12345678")
pub fn security_group_id() -> AttributeType {
    AttributeType::Custom {
        name: "SecurityGroupId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "sg")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Internet Gateway ID type (e.g., "igw-12345678")
pub fn internet_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "InternetGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "igw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Route Table ID type (e.g., "rtb-abcdef12")
pub fn route_table_id() -> AttributeType {
    AttributeType::Custom {
        name: "RouteTableId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "rtb")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// NAT Gateway ID type (e.g., "nat-0123456789abcdef0")
pub fn nat_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "NatGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "nat")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// VPC Peering Connection ID type (e.g., "pcx-12345678")
pub fn vpc_peering_connection_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcPeeringConnectionId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "pcx")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Transit Gateway ID type (e.g., "tgw-0123456789abcdef0")
pub fn transit_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "TransitGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "tgw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// VPN Gateway ID type (e.g., "vgw-12345678")
pub fn vpn_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpnGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vgw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Egress Only Internet Gateway ID type (e.g., "eigw-12345678")
pub fn egress_only_internet_gateway_id() -> AttributeType {
    AttributeType::Custom {
        name: "EgressOnlyInternetGatewayId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "eigw")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// VPC Endpoint ID type (e.g., "vpce-0123456789abcdef0")
pub fn vpc_endpoint_id() -> AttributeType {
    AttributeType::Custom {
        name: "VpcEndpointId".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_prefixed_resource_id(s, "vpce")
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

/// Availability Zone type (e.g., "us-east-1a", "ap-northeast-1c")
/// Validates format: region + single letter zone identifier
pub fn availability_zone() -> AttributeType {
    AttributeType::Custom {
        name: "AvailabilityZone".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                validate_availability_zone(s)
            } else {
                Err("Expected string".to_string())
            }
        },
        namespace: None,
    }
}

pub fn validate_availability_zone(az: &str) -> Result<(), String> {
    // Must end with a single lowercase letter (zone identifier)
    let zone_letter = az.chars().last();
    if !zone_letter.is_some_and(|c| c.is_ascii_lowercase()) {
        return Err(format!(
            "Invalid availability zone '{}': must end with a zone letter (a-z)",
            az
        ));
    }

    // Region part is everything except the last character
    let region = &az[..az.len() - 1];

    // Region must match pattern: lowercase-lowercase-digit
    // e.g., "us-east-1", "ap-northeast-1", "eu-west-2"
    let parts: Vec<&str> = region.split('-').collect();
    if parts.len() < 3 {
        return Err(format!(
            "Invalid availability zone '{}': expected format like 'us-east-1a'",
            az
        ));
    }

    // Last part of region must be a number
    let last = parts.last().unwrap();
    if last.parse::<u8>().is_err() {
        return Err(format!(
            "Invalid availability zone '{}': region must end with a number",
            az
        ));
    }

    // All other parts must be lowercase alphabetic
    for part in &parts[..parts.len() - 1] {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_lowercase()) {
            return Err(format!(
                "Invalid availability zone '{}': expected format like 'us-east-1a'",
                az
            ));
        }
    }

    Ok(())
}

/// IAM Policy Document type
/// Validates the structure of IAM policy documents (trust policies, inline policies, etc.)
pub fn iam_policy_document() -> AttributeType {
    AttributeType::Custom {
        name: "IamPolicyDocument".to_string(),
        base: Box::new(AttributeType::Map(Box::new(AttributeType::String))),
        validate: |value| validate_iam_policy_document(value),
        namespace: None,
    }
}

pub fn validate_iam_policy_document(value: &Value) -> Result<(), String> {
    let Value::Map(map) = value else {
        return Err("Expected a map for IAM policy document".to_string());
    };

    // Check Version if present
    if let Some(Value::String(v)) = map.get("version")
        && v != "2012-10-17"
        && v != "2008-10-17"
    {
        return Err(format!(
            "Invalid policy document Version '{}', expected '2012-10-17' or '2008-10-17'",
            v
        ));
    }

    // Check Statement if present
    if let Some(statement) = map.get("statement") {
        let Value::List(statements) = statement else {
            return Err("Policy document 'statement' must be a list".to_string());
        };

        for (i, stmt) in statements.iter().enumerate() {
            let Value::Map(stmt_map) = stmt else {
                return Err(format!("Statement[{}] must be a map", i));
            };

            // Validate Effect if present
            if let Some(Value::String(e)) = stmt_map.get("effect")
                && e != "Allow"
                && e != "Deny"
            {
                return Err(format!(
                    "Statement[{}] has invalid Effect '{}', expected 'Allow' or 'Deny'",
                    i, e
                ));
            }

            // Validate Sid if present (must be a string)
            if let Some(sid) = stmt_map.get("sid")
                && !matches!(sid, Value::String(_))
            {
                return Err(format!(
                    "Statement[{}] 'sid' must be a string", i
                ));
            }

            // Validate Action / NotAction if present (must be string or list of strings)
            for key in &["action", "not_action"] {
                if let Some(action) = stmt_map.get(*key) {
                    match action {
                        Value::String(_) => {}
                        Value::List(items) => {
                            for (j, item) in items.iter().enumerate() {
                                if !matches!(item, Value::String(_)) {
                                    return Err(format!(
                                        "Statement[{}] '{}[{}]' must be a string", i, key, j
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(format!(
                                "Statement[{}] '{}' must be a string or list of strings", i, key
                            ));
                        }
                    }
                }
            }

            // Validate Resource / NotResource if present (must be string or list of strings)
            for key in &["resource", "not_resource"] {
                if let Some(resource) = stmt_map.get(*key) {
                    match resource {
                        Value::String(_) => {}
                        Value::List(items) => {
                            for (j, item) in items.iter().enumerate() {
                                if !matches!(item, Value::String(_)) {
                                    return Err(format!(
                                        "Statement[{}] '{}[{}]' must be a string", i, key, j
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(format!(
                                "Statement[{}] '{}' must be a string or list of strings", i, key
                            ));
                        }
                    }
                }
            }

            // Validate Principal / NotPrincipal if present (must be string "*" or a map)
            for key in &["principal", "not_principal"] {
                if let Some(principal) = stmt_map.get(*key) {
                    match principal {
                        Value::String(_) => {}
                        Value::Map(_) => {}
                        _ => {
                            return Err(format!(
                                "Statement[{}] '{}' must be a string or a map", i, key
                            ));
                        }
                    }
                }
            }

            // Validate Condition if present (must be a map)
            if let Some(condition) = stmt_map.get("condition")
                && !matches!(condition, Value::Map(_))
            {
                return Err(format!(
                    "Statement[{}] 'condition' must be a map", i
                ));
            }
        }
    }

    Ok(())
}

EOF

# Add module declarations
for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    MODNAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-module-name)
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
    MODNAME=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-module-name)
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    FUNC_NAME="${FULL_RESOURCE}_config"

    echo "        ${MODNAME}::${FUNC_NAME}()," >> "$OUTPUT_DIR/mod.rs"
done

cat >> "$OUTPUT_DIR/mod.rs" << 'EOF'
    ]
}

/// Returns all generated schemas (for backward compatibility)
pub fn schemas() -> Vec<ResourceSchema> {
    configs().into_iter().map(|c| c.schema).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_arn_valid() {
        assert!(validate_arn("arn:aws:s3:::my-bucket").is_ok());
        assert!(validate_arn("arn:aws:iam::123456789012:role/MyRole").is_ok());
        assert!(validate_arn("arn:aws-cn:s3:::my-bucket").is_ok());
        assert!(validate_arn("arn:aws:ec2:us-east-1:123456789012:vpc/vpc-1234").is_ok());
    }

    #[test]
    fn validate_arn_invalid() {
        assert!(validate_arn("not-an-arn").is_err());
        assert!(validate_arn("arn:aws:s3").is_err());
        assert!(validate_arn("arn:aws").is_err());
        assert!(validate_arn("").is_err());
    }

    #[test]
    fn validate_arn_type_with_value() {
        let t = arn();
        assert!(
            t.validate(&Value::String("arn:aws:s3:::my-bucket".to_string()))
                .is_ok()
        );
        assert!(
            t.validate(&Value::String("not-an-arn".to_string()))
                .is_err()
        );
        assert!(t.validate(&Value::Int(42)).is_err());
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef("role".to_string(), "arn".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_aws_resource_id_valid() {
        assert!(validate_aws_resource_id("vpc-1a2b3c4d").is_ok());
        assert!(validate_aws_resource_id("subnet-0123456789abcdef0").is_ok());
        assert!(validate_aws_resource_id("sg-12345678").is_ok());
        assert!(validate_aws_resource_id("rtb-abcdef12").is_ok());
        assert!(validate_aws_resource_id("eipalloc-0123456789abcdef0").is_ok());
        assert!(validate_aws_resource_id("igw-12345678").is_ok());
    }

    #[test]
    fn validate_aws_resource_id_invalid() {
        assert!(validate_aws_resource_id("not-a-valid-id").is_err()); // hex part too short
        assert!(validate_aws_resource_id("vpc").is_err()); // no dash
        assert!(validate_aws_resource_id("vpc-short").is_err()); // hex part < 8
        assert!(validate_aws_resource_id("vpc-1234567").is_err()); // only 7 chars
        assert!(validate_aws_resource_id("VPC-12345678").is_err()); // uppercase prefix
    }

    #[test]
    fn validate_aws_resource_id_type_with_value() {
        let t = aws_resource_id();
        assert!(
            t.validate(&Value::String("vpc-1a2b3c4d".to_string()))
                .is_ok()
        );
        assert!(t.validate(&Value::String("vpc".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err());
        // ResourceRef should be accepted
        assert!(
            t.validate(&Value::ResourceRef(
                "my_vpc".to_string(),
                "vpc_id".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn validate_availability_zone_valid() {
        assert!(validate_availability_zone("us-east-1a").is_ok());
        assert!(validate_availability_zone("ap-northeast-1c").is_ok());
        assert!(validate_availability_zone("eu-central-1b").is_ok());
        assert!(validate_availability_zone("me-south-1a").is_ok());
        assert!(validate_availability_zone("us-west-2d").is_ok());
    }

    #[test]
    fn validate_availability_zone_invalid() {
        assert!(validate_availability_zone("us-east-1").is_err()); // no zone letter
        assert!(validate_availability_zone("US-EAST-1A").is_err()); // uppercase
        assert!(validate_availability_zone("us-east").is_err()); // no number
        assert!(validate_availability_zone("1a").is_err()); // too short
        assert!(validate_availability_zone("").is_err()); // empty
    }

    #[test]
    fn validate_availability_zone_type_with_value() {
        let t = availability_zone();
        assert!(t.validate(&Value::String("us-east-1a".to_string())).is_ok());
        assert!(t.validate(&Value::String("us-east-1".to_string())).is_err());
        assert!(t.validate(&Value::String("invalid".to_string())).is_err());
        assert!(t.validate(&Value::Int(42)).is_err());
    }

    #[test]
    fn validate_vpc_id_valid() {
        let t = vpc_id();
        assert!(t.validate(&Value::String("vpc-1a2b3c4d".to_string())).is_ok());
        assert!(t
            .validate(&Value::String("vpc-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_vpc_id_invalid() {
        let t = vpc_id();
        assert!(t
            .validate(&Value::String("subnet-12345678".to_string()))
            .is_err());
        assert!(t.validate(&Value::String("vpc-short".to_string())).is_err());
        assert!(t.validate(&Value::String("vpc".to_string())).is_err());
    }

    #[test]
    fn validate_subnet_id_valid() {
        let t = subnet_id();
        assert!(t
            .validate(&Value::String("subnet-0123456789abcdef0".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("subnet-12345678".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_subnet_id_invalid() {
        let t = subnet_id();
        assert!(t
            .validate(&Value::String("vpc-12345678".to_string()))
            .is_err());
        assert!(t
            .validate(&Value::String("subnet-short".to_string()))
            .is_err());
    }

    #[test]
    fn validate_security_group_id_valid() {
        let t = security_group_id();
        assert!(t
            .validate(&Value::String("sg-12345678".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("sg-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_security_group_id_invalid() {
        let t = security_group_id();
        assert!(t
            .validate(&Value::String("vpc-12345678".to_string()))
            .is_err());
        assert!(t.validate(&Value::String("sg-short".to_string())).is_err());
    }

    #[test]
    fn validate_internet_gateway_id_valid() {
        let t = internet_gateway_id();
        assert!(t
            .validate(&Value::String("igw-12345678".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("igw-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_route_table_id_valid() {
        let t = route_table_id();
        assert!(t
            .validate(&Value::String("rtb-abcdef12".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("rtb-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_nat_gateway_id_valid() {
        let t = nat_gateway_id();
        assert!(t
            .validate(&Value::String("nat-0123456789abcdef0".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("nat-12345678".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_vpc_peering_connection_id_valid() {
        let t = vpc_peering_connection_id();
        assert!(t
            .validate(&Value::String("pcx-12345678".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("pcx-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_transit_gateway_id_valid() {
        let t = transit_gateway_id();
        assert!(t
            .validate(&Value::String("tgw-0123456789abcdef0".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("tgw-12345678".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_vpn_gateway_id_valid() {
        let t = vpn_gateway_id();
        assert!(t
            .validate(&Value::String("vgw-12345678".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("vgw-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_egress_only_internet_gateway_id_valid() {
        let t = egress_only_internet_gateway_id();
        assert!(t
            .validate(&Value::String("eigw-12345678".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("eigw-0123456789abcdef0".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_vpc_endpoint_id_valid() {
        let t = vpc_endpoint_id();
        assert!(t
            .validate(&Value::String("vpce-0123456789abcdef0".to_string()))
            .is_ok());
        assert!(t
            .validate(&Value::String("vpce-12345678".to_string()))
            .is_ok());
    }

    #[test]
    fn validate_iam_policy_document_valid() {
        let doc = Value::Map(
            vec![
                ("version".to_string(), Value::String("2012-10-17".to_string())),
                ("statement".to_string(), Value::List(vec![
                    Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            ("principal".to_string(), Value::Map(
                                vec![("service".to_string(), Value::String("ec2.amazonaws.com".to_string()))]
                                    .into_iter().collect()
                            )),
                            ("action".to_string(), Value::String("sts:AssumeRole".to_string())),
                        ].into_iter().collect()
                    ),
                ])),
            ].into_iter().collect()
        );
        assert!(validate_iam_policy_document(&doc).is_ok());
    }

    #[test]
    fn validate_iam_policy_document_invalid_version() {
        let doc = Value::Map(
            vec![
                ("version".to_string(), Value::String("2023-01-01".to_string())),
            ].into_iter().collect()
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("2012-10-17"));
    }

    #[test]
    fn validate_iam_policy_document_invalid_effect() {
        let doc = Value::Map(
            vec![
                ("statement".to_string(), Value::List(vec![
                    Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Maybe".to_string())),
                        ].into_iter().collect()
                    ),
                ])),
            ].into_iter().collect()
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Maybe"));
    }

    #[test]
    fn validate_iam_policy_document_not_a_map() {
        let result = validate_iam_policy_document(&Value::String("not a map".to_string()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Expected a map"));
    }

    #[test]
    fn validate_iam_policy_document_type_with_resource_ref() {
        let t = iam_policy_document();
        // ResourceRef should be accepted (via Custom type handling in schema.rs)
        assert!(
            t.validate(&Value::ResourceRef("role".to_string(), "policy".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn validate_iam_policy_document_valid_with_all_fields() {
        let doc = Value::Map(
            vec![
                ("version".to_string(), Value::String("2012-10-17".to_string())),
                ("statement".to_string(), Value::List(vec![
                    Value::Map(
                        vec![
                            ("sid".to_string(), Value::String("AllowS3Access".to_string())),
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            ("principal".to_string(), Value::Map(
                                vec![("service".to_string(), Value::String("ec2.amazonaws.com".to_string()))]
                                    .into_iter().collect()
                            )),
                            ("action".to_string(), Value::List(vec![
                                Value::String("s3:GetObject".to_string()),
                                Value::String("s3:PutObject".to_string()),
                            ])),
                            ("resource".to_string(), Value::String("*".to_string())),
                            ("condition".to_string(), Value::Map(
                                vec![("string_equals".to_string(), Value::Map(
                                    vec![("aws:RequestedRegion".to_string(), Value::String("us-east-1".to_string()))]
                                        .into_iter().collect()
                                ))]
                                    .into_iter().collect()
                            )),
                        ].into_iter().collect()
                    ),
                ])),
            ].into_iter().collect()
        );
        assert!(validate_iam_policy_document(&doc).is_ok());
    }

    #[test]
    fn validate_iam_policy_document_invalid_principal() {
        let doc = Value::Map(
            vec![
                ("statement".to_string(), Value::List(vec![
                    Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            ("principal".to_string(), Value::List(vec![
                                Value::String("not-valid".to_string()),
                            ])),
                        ].into_iter().collect()
                    ),
                ])),
            ].into_iter().collect()
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("principal"));
    }

    #[test]
    fn validate_iam_policy_document_invalid_action() {
        let doc = Value::Map(
            vec![
                ("statement".to_string(), Value::List(vec![
                    Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            ("action".to_string(), Value::Int(42)),
                        ].into_iter().collect()
                    ),
                ])),
            ].into_iter().collect()
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("action"));
    }

    #[test]
    fn validate_iam_policy_document_invalid_condition() {
        let doc = Value::Map(
            vec![
                ("statement".to_string(), Value::List(vec![
                    Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            ("condition".to_string(), Value::String("not-a-map".to_string())),
                        ].into_iter().collect()
                    ),
                ])),
            ].into_iter().collect()
        );
        let result = validate_iam_policy_document(&doc);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("condition"));
    }

    #[test]
    fn validate_prefix_mismatch_error_messages() {
        let t = vpc_id();
        let result = t.validate(&Value::String("subnet-12345678".to_string()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("vpc-xxxxxxxx"));
        assert!(err_msg.contains("subnet-12345678"));
    }
}
EOF

echo ""
echo "Running cargo fmt..."
cargo fmt -p carina-provider-awscc

echo ""
echo "Done! Generated schemas in $OUTPUT_DIR"
