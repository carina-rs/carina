---
title: "awscc.ec2.TransitGateway"
description: "AWSCC EC2 TransitGateway resource reference"
---


CloudFormation Type: `AWS::EC2::TransitGateway`

Resource Type definition for AWS::EC2::TransitGateway

## Example

```crn
awscc.ec2.TransitGateway {
  description = 'Example Transit Gateway'

  tags = {
    Environment = 'example'
  }
}
```

## Argument Reference

### `amazon_side_asn`

- **Type:** Int(1..=4294967294)
- **Required:** No
- **Create-only:** Yes

### `association_default_route_table_id`

- **Type:** TgwRouteTableId
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
- **Write-only:** Yes

### `multicast_support`

- **Type:** [Enum (MulticastSupport)](#multicast_support-multicastsupport)
- **Required:** No
- **Create-only:** Yes

### `propagation_default_route_table_id`

- **Type:** TgwRouteTableId
- **Required:** No

### `security_group_referencing_support`

- **Type:** [Enum (SecurityGroupReferencingSupport)](#security_group_referencing_support-securitygroupreferencingsupport)
- **Required:** No

### `tags`

- **Type:** Map(String)
- **Required:** No

### `transit_gateway_cidr_blocks`

- **Type:** `List<Cidr>`
- **Required:** No

### `vpn_ecmp_support`

- **Type:** [Enum (VpnEcmpSupport)](#vpn_ecmp_support-vpnecmpsupport)
- **Required:** No

## Enum Values

### auto_accept_shared_attachments (AutoAcceptSharedAttachments)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.AutoAcceptSharedAttachments.enable` |
| `disable` | `awscc.ec2.TransitGateway.AutoAcceptSharedAttachments.disable` |

Shorthand formats: `enable` or `AutoAcceptSharedAttachments.enable`

### default_route_table_association (DefaultRouteTableAssociation)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.DefaultRouteTableAssociation.enable` |
| `disable` | `awscc.ec2.TransitGateway.DefaultRouteTableAssociation.disable` |

Shorthand formats: `enable` or `DefaultRouteTableAssociation.enable`

### default_route_table_propagation (DefaultRouteTablePropagation)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.DefaultRouteTablePropagation.enable` |
| `disable` | `awscc.ec2.TransitGateway.DefaultRouteTablePropagation.disable` |

Shorthand formats: `enable` or `DefaultRouteTablePropagation.enable`

### dns_support (DnsSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.DnsSupport.enable` |
| `disable` | `awscc.ec2.TransitGateway.DnsSupport.disable` |

Shorthand formats: `enable` or `DnsSupport.enable`

### encryption_support (EncryptionSupport)

| Value | DSL Identifier |
|-------|----------------|
| `disable` | `awscc.ec2.TransitGateway.EncryptionSupport.disable` |
| `enable` | `awscc.ec2.TransitGateway.EncryptionSupport.enable` |

Shorthand formats: `disable` or `EncryptionSupport.disable`

### encryption_support_state (EncryptionSupportState)

| Value | DSL Identifier |
|-------|----------------|
| `disable` | `awscc.ec2.TransitGateway.EncryptionSupportState.disable` |
| `enable` | `awscc.ec2.TransitGateway.EncryptionSupportState.enable` |

Shorthand formats: `disable` or `EncryptionSupportState.disable`

### multicast_support (MulticastSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.MulticastSupport.enable` |
| `disable` | `awscc.ec2.TransitGateway.MulticastSupport.disable` |

Shorthand formats: `enable` or `MulticastSupport.enable`

### security_group_referencing_support (SecurityGroupReferencingSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.SecurityGroupReferencingSupport.enable` |
| `disable` | `awscc.ec2.TransitGateway.SecurityGroupReferencingSupport.disable` |

Shorthand formats: `enable` or `SecurityGroupReferencingSupport.enable`

### vpn_ecmp_support (VpnEcmpSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.TransitGateway.VpnEcmpSupport.enable` |
| `disable` | `awscc.ec2.TransitGateway.VpnEcmpSupport.disable` |

Shorthand formats: `enable` or `VpnEcmpSupport.enable`

## Attribute Reference

### `encryption_support_state`

- **Type:** [Enum (EncryptionSupportState)](#encryption_support_state-encryptionsupportstate)

### `id`

- **Type:** TransitGatewayId

### `transit_gateway_arn`

- **Type:** Arn

