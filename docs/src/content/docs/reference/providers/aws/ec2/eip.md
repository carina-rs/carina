---
title: "aws.ec2.Eip"
description: "AWS EC2 eip resource reference"
---


CloudFormation Type: `AWS::EC2::EIP`

Describes an Elastic IP address, or a carrier IP address.

## Argument Reference

### `address`

- **Type:** String
- **Required:** No

The Elastic IP address to recover or an IPv4 address from an address pool.

### `domain`

- **Type:** [Enum (Domain)](#domain-domain)
- **Required:** No

The network (vpc).

### `public_ipv4_pool`

- **Type:** String
- **Required:** No

The ID of an address pool that you own. Use this parameter to let Amazon EC2 select an address from the address pool. To specify a specific address from the address pool, use the Address parameter instead.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### domain (Domain)

| Value | DSL Identifier |
|-------|----------------|
| `standard` | `aws.ec2.Eip.Domain.standard` |
| `vpc` | `aws.ec2.Eip.Domain.vpc` |

Shorthand formats: `standard` or `Domain.standard`

## Attribute Reference

### `allocation_id`

- **Type:** AllocationId

### `public_ip`

- **Type:** Ipv4Address

