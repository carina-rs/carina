# Type Compatibility with Subtype Chains

## Goal

Enable proper type compatibility checking for exports and module attributes by leveraging subtype relationships encoded in `AttributeType::Custom { base }` chains.

## Current State

All Custom types in `carina-aws-types` have `base: Box::new(AttributeType::String)`, flattening the type hierarchy. For example:

```
KmsKeyArn { base: String }    // should be: KmsKeyArn { base: Arn }
IamRoleArn { base: String }   // should be: IamRoleArn { base: Arn }
Arn { base: String }           // correct
VpcId { base: String }         // should be: VpcId { base: AwsResourceId }
AwsResourceId { base: String } // correct
```

This means `is_type_expr_compatible_with_schema(Simple("arn"), Custom("KmsKeyArn"))` cannot determine that `KmsKeyArn` is a subtype of `Arn`.

## Design

### 1. Define subtype relationships in carina-aws-types

Change `base` from `String` to the appropriate parent Custom type:

**ARN group:**
- `Arn { base: String }` (root)
- `IamRoleArn { base: Arn }` 
- `IamPolicyArn { base: Arn }`
- `KmsKeyArn { base: Arn }`

**ResourceId group:**
- `AwsResourceId { base: String }` (root)
- `VpcId { base: AwsResourceId }`
- `SubnetId { base: AwsResourceId }`
- `InstanceId { base: AwsResourceId }`
- `SecurityGroupId { base: AwsResourceId }`
- `RouteTableId { base: AwsResourceId }`
- `NetworkInterfaceId { base: AwsResourceId }`
- `NetworkAclId { base: AwsResourceId }`
- `InternetGatewayId { base: AwsResourceId }`
- `NatGatewayId { base: AwsResourceId }`
- `TransitGatewayId { base: AwsResourceId }`
- `TransitGatewayAttachmentId { base: AwsResourceId }`
- `VpcEndpointId { base: AwsResourceId }`
- `VpcPeeringConnectionId { base: AwsResourceId }`
- `VpnGatewayId { base: AwsResourceId }`
- `EgressOnlyInternetGatewayId { base: AwsResourceId }`
- `CarrierGatewayId { base: AwsResourceId }`
- `LocalGatewayId { base: AwsResourceId }`
- `PrefixListId { base: AwsResourceId }`
- `IpamId { base: AwsResourceId }`
- `IpamPoolId { base: AwsResourceId }`
- `FlowLogId { base: AwsResourceId }`
- `AllocationId { base: AwsResourceId }`
- `TgwRouteTableId { base: AwsResourceId }`
- `SubnetRouteTableAssociationId { base: AwsResourceId }`
- `VpcCidrBlockAssociationId { base: AwsResourceId }`
- `SecurityGroupRuleId { base: AwsResourceId }`

**KMS group:**
- `KmsKeyId { base: AwsResourceId }`

**IAM group:**
- `IamRoleId { base: AwsResourceId }`

**Independent types** (no subtype relationship, base stays String):
- `AwsAccountId { base: String }`
- `AvailabilityZoneId { base: String }`
- `IamPolicyDocument { base: String }` (actually JSON, but treated as string)
- `IamPolicyEffect { base: String }`
- `IamPolicyPrincipal { base: String }`
- `IamPolicyStatement { base: String }`
- `IamPolicyVersion { base: String }`
- `ConditionOperator { base: String }`

### 2. Update `is_type_expr_compatible_with_schema` in carina-core

Walk the base chain to check subtype compatibility:

```rust
TypeExpr::Simple(name) => {
    // Walk the base chain: if any type in the chain matches, it's compatible
    let mut current = attr_type;
    loop {
        let type_snake = pascal_to_snake(&current.type_name());
        if &type_snake == name {
            return true;
        }
        match current {
            AttributeType::Custom { base, .. } => current = base,
            AttributeType::String => {
                // Reached String root. Only compatible if name matches
                // a base type we already checked.
                return false;
            }
            _ => return false,
        }
    }
}
```

This enables:
- `arn` accepts `KmsKeyArn` (chain: KmsKeyArn → Arn ✓)
- `kms_key_arn` rejects `IamRoleArn` (chain: IamRoleArn → Arn → String, none match `kms_key_arn`)
- `aws_resource_id` accepts `VpcId` (chain: VpcId → AwsResourceId ✓)
- `string` accepts everything (TypeExpr::String is handled separately)
- `aws_account_id` rejects `InstanceId` (chain: InstanceId → AwsResourceId → String, none match `aws_account_id`)

### 3. Affected files

**carina-aws-types (in both provider repos):**
- `carina-aws-types/src/lib.rs` — update `base` fields for all Custom types

**carina-core:**
- `carina-core/src/validation.rs` — update `is_type_expr_compatible_with_schema` to walk base chain

**No LSP changes needed** — the LSP already calls `is_type_expr_compatible_with_schema`.

## Edge Cases

- **Plain String schema type**: `TypeExpr::Simple("aws_account_id")` vs `AttributeType::String` — incompatible (String is not AwsAccountId). This is correct because if the schema says the attribute is a plain String, it shouldn't be assumed to be a specific semantic type.
- **Self-match**: `TypeExpr::Simple("arn")` vs `AttributeType::Custom { name: "Arn" }` — compatible (exact match).
- **Deep chains**: If we ever have 3+ levels (e.g., `IamManagedPolicyArn { base: IamPolicyArn { base: Arn } }`), the chain walk handles it naturally.
