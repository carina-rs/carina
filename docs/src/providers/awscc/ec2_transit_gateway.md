# awscc.ec2_transit_gateway

CloudFormation Type: `AWS::EC2::TransitGateway`

Resource Type definition for AWS::EC2::TransitGateway

## Argument Reference

### `amazon_side_asn`

- **Type:** Int
- **Required:** No

### `association_default_route_table_id`

- **Type:** AwsResourceId
- **Required:** No

### `auto_accept_shared_attachments`

- **Type:** String
- **Required:** No

### `default_route_table_association`

- **Type:** String
- **Required:** No

### `default_route_table_propagation`

- **Type:** String
- **Required:** No

### `description`

- **Type:** String
- **Required:** No

### `dns_support`

- **Type:** String
- **Required:** No

### `encryption_support`

- **Type:** Enum (EncryptionSupport)
- **Required:** No

### `multicast_support`

- **Type:** String
- **Required:** No

### `propagation_default_route_table_id`

- **Type:** AwsResourceId
- **Required:** No

### `security_group_referencing_support`

- **Type:** String
- **Required:** No

### `tags`

- **Type:** Map
- **Required:** No

### `transit_gateway_cidr_blocks`

- **Type:** List<String>
- **Required:** No

### `vpn_ecmp_support`

- **Type:** String
- **Required:** No

## Attribute Reference

### `encryption_support_state`

- **Type:** String

### `id`

- **Type:** String

### `transit_gateway_arn`

- **Type:** Arn

## Enum Values

### encryption_support (EncryptionSupport)

| Value | DSL Identifier |
|-------|----------------|
| `disable` | `awscc.ec2_transit_gateway.EncryptionSupport.disable` |
| `enable` | `awscc.ec2_transit_gateway.EncryptionSupport.enable` |

Shorthand formats: `disable` or `EncryptionSupport.disable`



## Example

```crn
awscc.ec2_transit_gateway {
  name        = "example-tgw"
  description = "Example Transit Gateway"

  tags = {
    Environment = "example"
  }
}
```
