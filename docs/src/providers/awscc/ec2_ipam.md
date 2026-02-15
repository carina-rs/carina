# awscc.ec2_ipam

CloudFormation Type: `AWS::EC2::IPAM`

Resource Schema of AWS::EC2::IPAM Type

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

- **Type:** Map
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
| `ipam-owner` | `awscc.ec2_ipam.MeteredAccount.ipam-owner` |
| `resource-owner` | `awscc.ec2_ipam.MeteredAccount.resource-owner` |

Shorthand formats: `ipam-owner` or `MeteredAccount.ipam-owner`

### tier (Tier)

| Value | DSL Identifier |
|-------|----------------|
| `free` | `awscc.ec2_ipam.Tier.free` |
| `advanced` | `awscc.ec2_ipam.Tier.advanced` |

Shorthand formats: `free` or `Tier.free`

## Struct Definitions

### IpamOperatingRegion

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `region_name` | String | Yes | The name of the region. |

### IpamOrganizationalUnitExclusion

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `organizations_entity_path` | String | Yes | An AWS Organizations entity path. Build the path for the OU(s) using AWS Organizations IDs separated... |

## Attribute Reference

### `arn`

- **Type:** Arn

### `default_resource_discovery_association_id`

- **Type:** String

### `default_resource_discovery_id`

- **Type:** String

### `ipam_id`

- **Type:** String

### `private_default_scope_id`

- **Type:** String

### `public_default_scope_id`

- **Type:** String

### `resource_discovery_association_count`

- **Type:** Int

### `scope_count`

- **Type:** Int



## Example

```crn
let ipam = awscc.ec2_ipam {
  description = "Example IPAM"
  tier        = free

  operating_regions = [
    {
      region_name = "ap-northeast-1"
    }
  ]

  tags = {
    Environment = "example"
  }
}
```
