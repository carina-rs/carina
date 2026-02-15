# awscc.ec2_transit_gateway_attachment

CloudFormation Type: `AWS::EC2::TransitGatewayAttachment`

Resource Type definition for AWS::EC2::TransitGatewayAttachment

## Argument Reference

### `options`

- **Type:** [Struct(Options)](#options)
- **Required:** No

The options for the transit gateway vpc attachment.

### `subnet_ids`

- **Type:** `List<SubnetId>`
- **Required:** Yes

### `tags`

- **Type:** Map
- **Required:** No

### `transit_gateway_id`

- **Type:** TransitGatewayId
- **Required:** Yes

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

## Struct Definitions

### Options

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `appliance_mode_support` | String | No | Indicates whether to enable Ipv6 Support for Vpc Attachment. Valid Values: enable | disable |
| `dns_support` | Enum | No | Indicates whether to enable DNS Support for Vpc Attachment. Valid Values: enable | disable |
| `ipv6_support` | String | No | Indicates whether to enable Ipv6 Support for Vpc Attachment. Valid Values: enable | disable |
| `security_group_referencing_support` | Enum | No | Indicates whether to enable Security Group referencing support for Vpc Attachment. Valid Values: ena... |

## Attribute Reference

### `id`

- **Type:** String


