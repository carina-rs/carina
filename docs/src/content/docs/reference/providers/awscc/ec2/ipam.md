---
title: "awscc.ec2.ipam"
description: "AWSCC EC2 ipam resource reference"
---


CloudFormation Type: `AWS::EC2::IPAM`

Resource Schema of AWS::EC2::IPAM Type

## Example

```crn
awscc.ec2.ipam {
  description = 'Example IPAM'
  tier        = free

  operating_region {
    region_name = 'ap-northeast-1'
  }

  tags = {
    Environment = 'example'
  }
}
```

## Argument Reference

### `default_resource_discovery_organizational_unit_exclusions`

- **Type:** [List\<IpamOrganizationalUnitExclusion\>](#ipamorganizationalunitexclusion)
- **Required:** No

A set of organizational unit (OU) exclusions for the default resource discovery, created with this IPAM.

### `description`

- **Type:** String
- **Required:** No

### `enable_private_gua`

- **Type:** Bool
- **Required:** No

Enable provisioning of GUA space in private pools.

### `metered_account`

- **Type:** [Enum (MeteredAccount)](#metered_account-meteredaccount)
- **Required:** No

A metered account is an account that is charged for active IP addresses managed in IPAM

### `operating_regions`

- **Type:** [List\<IpamOperatingRegion\>](#ipamoperatingregion)
- **Required:** No

The regions IPAM is enabled for. Allows pools to be created in these regions, as well as enabling monitoring

### `tags`

- **Type:** Map(String)
- **Required:** No

An array of key-value pairs to apply to this resource.

### `tier`

- **Type:** [Enum (Tier)](#tier-tier)
- **Required:** No

The tier of the IPAM.

## Enum Values

### metered_account (MeteredAccount)

| Value | DSL Identifier |
|-------|----------------|
| `ipam-owner` | `awscc.ec2.ipam.MeteredAccount.ipam_owner` |
| `resource-owner` | `awscc.ec2.ipam.MeteredAccount.resource_owner` |

Shorthand formats: `ipam_owner` or `MeteredAccount.ipam_owner`

### tier (Tier)

| Value | DSL Identifier |
|-------|----------------|
| `free` | `awscc.ec2.ipam.Tier.free` |
| `advanced` | `awscc.ec2.ipam.Tier.advanced` |

Shorthand formats: `free` or `Tier.free`

## Struct Definitions

### IpamOperatingRegion

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `region_name` | Region | Yes | The name of the region. |

### IpamOrganizationalUnitExclusion

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `organizations_entity_path` | String(len: 1..) | Yes | An AWS Organizations entity path. Build the path for the OU(s) using AWS Organizations IDs separated by a '/'. Include all child OUs by ending the path with '/*'. |

## Attribute Reference

### `arn`

- **Type:** Arn

The Amazon Resource Name (ARN) of the IPAM.

### `default_resource_discovery_association_id`

- **Type:** String

The Id of the default association to the default resource discovery, created with this IPAM.

### `default_resource_discovery_id`

- **Type:** String

The Id of the default resource discovery, created with this IPAM.

### `ipam_id`

- **Type:** IpamId

Id of the IPAM.

### `private_default_scope_id`

- **Type:** String

The Id of the default scope for publicly routable IP space, created with this IPAM.

### `public_default_scope_id`

- **Type:** String(len: ..=255)

The Id of the default scope for publicly routable IP space, created with this IPAM.

### `resource_discovery_association_count`

- **Type:** Int

The count of resource discoveries associated with this IPAM.

### `scope_count`

- **Type:** Int

The number of scopes that currently exist in this IPAM.

