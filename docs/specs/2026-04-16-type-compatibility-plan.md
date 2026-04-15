# Type Compatibility Implementation Plan

## Task 1/3: Update base chains in carina-aws-types

**Repo**: carina-provider-aws, carina-provider-awscc (both share carina-aws-types)
**File**: `carina-aws-types/src/lib.rs`

Change `base: Box::new(AttributeType::String)` to proper parent types for:
- ARN subtypes → `base: Box::new(arn())`
- ResourceId subtypes → `base: Box::new(aws_resource_id())`
- KmsKeyId, IamRoleId → `base: Box::new(aws_resource_id())`

Keep `base: String` for independent types (AwsAccountId, AvailabilityZoneId, IAM policy types, ConditionOperator).

**Test**: Existing acceptance tests must pass. Add unit tests verifying the base chain (e.g., `kms_key_arn().base == Arn`, `arn().base == String`).

## Task 2/3: Walk base chain in is_type_expr_compatible_with_schema

**Repo**: carina (this repo)
**File**: `carina-core/src/validation.rs`

Update `TypeExpr::Simple(name)` arm to walk the `base` chain instead of using `is_string_compatible_type`:

```rust
TypeExpr::Simple(name) => {
    let mut current = attr_type;
    loop {
        let type_snake = pascal_to_snake(&current.type_name());
        if &type_snake == name {
            return true;
        }
        match current {
            AttributeType::Custom { base, .. } => current = base,
            _ => return false,
        }
    }
}
```

**Test**:
- `Simple("arn")` vs `Custom("KmsKeyArn", base: Arn)` → true
- `Simple("kms_key_arn")` vs `Custom("IamRoleArn", base: Arn)` → false
- `Simple("aws_resource_id")` vs `Custom("VpcId", base: AwsResourceId)` → true
- `Simple("vpc_id")` vs `Custom("SubnetId", base: AwsResourceId)` → false
- `Simple("string")` is handled by TypeExpr::String, not Simple

## Task 3/3: Verify end-to-end

**Repos**: All three (carina, carina-provider-aws, carina-provider-awscc)

- `carina validate` on infra repo's identity-center directory
- LSP diagnostics with exports referencing cross-file bindings
- No regressions in existing acceptance tests
