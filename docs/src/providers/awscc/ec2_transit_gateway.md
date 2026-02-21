# awscc.ec2.transit_gateway

CloudFormation Type: `AWS::EC2::TransitGateway`

Resource Type definition for AWS::EC2::TransitGateway

## Argument Reference

### `amazon_side_asn`

- **Type:** Int
- **Required:** No

### `association_default_route_table_id`

- **Type:** RouteTableId
- **Required:** No

### `auto_accept_shared_attachments`

- **Type:** [Enum (AutoAcceptSharedAttachments)](#auto_accept_shared_attachments-autoacceptsharedattachments)
- **Required:** No

### `default_route_table_association`

- **Type:** [Enum (DefaultRouteTableAssociation)](#default_route_table_association-defaultroutetableassociation)
- **Required:** No

### `default_route_table_propagation`

- **Type:** [Enum (DefaultRouteTablePropagation)](#default_route_table_propagation-defaultroutetablepropagation)
- **Required:** No

### `description`

- **Type:** String
- **Required:** No

### `dns_support`

- **Type:** [Enum (DnsSupport)](#dns_support-dnssupport)
- **Required:** No

### `encryption_support`

- **Type:** [Enum (EncryptionSupport)](#encryption_support-encryptionsupport)
- **Required:** No

### `multicast_support`

- **Type:** [Enum (MulticastSupport)](#multicast_support-multicastsupport)
- **Required:** No

### `propagation_default_route_table_id`

- **Type:** RouteTableId
- **Required:** No

### `security_group_referencing_support`

- **Type:** [Enum (SecurityGroupReferencingSupport)](#security_group_referencing_support-securitygroupreferencingsupport)
- **Required:** No

### `tags`

- **Type:** Map
- **Required:** No

### `transit_gateway_cidr_blocks`

- **Type:** `List<Ipv4Cidr>`
- **Required:** No

### `vpn_ecmp_support`

- **Type:** [Enum (VpnEcmpSupport)](#vpn_ecmp_support-vpnecmpsupport)
- **Required:** No

## Enum Values

### auto_accept_shared_attachments (AutoAcceptSharedAttachments)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.AutoAcceptSharedAttachments.enable` |
| `disable` | `awscc.ec2.transit_gateway.AutoAcceptSharedAttachments.disable` |

Shorthand formats: `enable` or `AutoAcceptSharedAttachments.enable`

### default_route_table_association (DefaultRouteTableAssociation)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.DefaultRouteTableAssociation.enable` |
| `disable` | `awscc.ec2.transit_gateway.DefaultRouteTableAssociation.disable` |

Shorthand formats: `enable` or `DefaultRouteTableAssociation.enable`

### default_route_table_propagation (DefaultRouteTablePropagation)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.DefaultRouteTablePropagation.enable` |
| `disable` | `awscc.ec2.transit_gateway.DefaultRouteTablePropagation.disable` |

Shorthand formats: `enable` or `DefaultRouteTablePropagation.enable`

### dns_support (DnsSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.DnsSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway.DnsSupport.disable` |

Shorthand formats: `enable` or `DnsSupport.enable`

### encryption_support (EncryptionSupport)

| Value | DSL Identifier |
|-------|----------------|
| `disable` | `awscc.ec2.transit_gateway.EncryptionSupport.disable` |
| `enable` | `awscc.ec2.transit_gateway.EncryptionSupport.enable` |

Shorthand formats: `disable` or `EncryptionSupport.disable`

### multicast_support (MulticastSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.MulticastSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway.MulticastSupport.disable` |

Shorthand formats: `enable` or `MulticastSupport.enable`

### security_group_referencing_support (SecurityGroupReferencingSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.SecurityGroupReferencingSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway.SecurityGroupReferencingSupport.disable` |

Shorthand formats: `enable` or `SecurityGroupReferencingSupport.enable`

### vpn_ecmp_support (VpnEcmpSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway.VpnEcmpSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway.VpnEcmpSupport.disable` |

Shorthand formats: `enable` or `VpnEcmpSupport.enable`

## Attribute Reference

### `encryption_support_state`

- **Type:** String

### `id`

- **Type:** String

### `transit_gateway_arn`

- **Type:** Arn



## Example

```crn
let tgw = awscc.ec2.transit_gateway {
  description = "Example Transit Gateway"

  tags = {
    Environment = "example"
  }
}
```
