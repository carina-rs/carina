---
title: "aws.ec2.vpn_gateway"
description: "AWS EC2 vpn_gateway resource reference"
---


CloudFormation Type: `AWS::EC2::VPNGateway`

Describes a virtual private gateway.

## Argument Reference

### `amazon_side_asn`

- **Type:** Int
- **Required:** No

A private Autonomous System Number (ASN) for the Amazon side of a BGP session. If you're using a 16-bit ASN, it must be in the 64512 to 65534 range. If you're using a 32-bit ASN, it must be in the 4200000000 to 4294967294 range. Default: 64512

### `availability_zone`

- **Type:** AvailabilityZone
- **Required:** No

The Availability Zone for the virtual private gateway.

### `type`

- **Type:** [Enum (Type)](#type-type)
- **Required:** Yes

The type of VPN connection this virtual private gateway supports.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### type (Type)

| Value | DSL Identifier |
|-------|----------------|
| `ipsec.1` | `aws.ec2.vpn_gateway.Type.ipsec.1` |

Shorthand formats: `ipsec.1` or `Type.ipsec.1`

## Attribute Reference

### `vpn_gateway_id`

- **Type:** vpn_gateway_id

