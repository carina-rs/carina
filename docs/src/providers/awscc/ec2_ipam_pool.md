# awscc.ec2_ipam_pool

CloudFormation Type: `AWS::EC2::IPAMPool`

Resource Schema of AWS::EC2::IPAMPool Type

## Argument Reference

### `address_family`

- **Type:** [Enum (AddressFamily)](#address_family-addressfamily)
- **Required:** Yes

The address family of the address space in this pool. Either IPv4 or IPv6.

### `allocation_default_netmask_length`

- **Type:** Int
- **Required:** No

The default netmask length for allocations made from this pool. This value is used when the netmask length of an allocation isn't specified.

### `allocation_max_netmask_length`

- **Type:** Int
- **Required:** No

The maximum allowed netmask length for allocations made from this pool.

### `allocation_min_netmask_length`

- **Type:** Int
- **Required:** No

The minimum allowed netmask length for allocations made from this pool.

### `allocation_resource_tags`

- **Type:** `List<Map>`
- **Required:** No

When specified, an allocation will not be allowed unless a resource has a matching set of tags.

### `auto_import`

- **Type:** Bool
- **Required:** No

Determines what to do if IPAM discovers resources that haven't been assigned an allocation. If set to true, an allocation will be made automatically.

### `aws_service`

- **Type:** [Enum (AwsService)](#aws_service-awsservice)
- **Required:** No

Limits which service in Amazon Web Services that the pool can be used in.

### `description`

- **Type:** String
- **Required:** No

### `ipam_scope_id`

- **Type:** String
- **Required:** Yes

The Id of the scope this pool is a part of.

### `locale`

- **Type:** String
- **Required:** No

The region of this pool. If not set, this will default to "None" which will disable non-custom allocations. If the locale has been specified for the source pool, this value must match.

### `provisioned_cidrs`

- **Type:** [List\<ProvisionedCidr\>](#provisionedcidr)
- **Required:** No

A list of cidrs representing the address space available for allocation in this pool.

### `public_ip_source`

- **Type:** [Enum (PublicIpSource)](#public_ip_source-publicipsource)
- **Required:** No

The IP address source for pools in the public scope. Only used for provisioning IP address CIDRs to pools in the public scope. Default is `byoip`.

### `publicly_advertisable`

- **Type:** Bool
- **Required:** No

Determines whether or not address space from this pool is publicly advertised. Must be set if and only if the pool is IPv6.

### `source_ipam_pool_id`

- **Type:** IpamPoolId
- **Required:** No

The Id of this pool's source. If set, all space provisioned in this pool must be free space provisioned in the parent pool.

### `source_resource`

- **Type:** [Struct(SourceResource)](#sourceresource)
- **Required:** No

### `tags`

- **Type:** Map
- **Required:** No

An array of key-value pairs to apply to this resource.

## Enum Values

### address_family (AddressFamily)

| Value | DSL Identifier |
|-------|----------------|
| `IPv4` | `awscc.ec2_ipam_pool.AddressFamily.IPv4` |
| `IPv6` | `awscc.ec2_ipam_pool.AddressFamily.IPv6` |

Shorthand formats: `IPv4` or `AddressFamily.IPv4`

### aws_service (AwsService)

| Value | DSL Identifier |
|-------|----------------|
| `ec2` | `awscc.ec2_ipam_pool.AwsService.ec2` |
| `global-services` | `awscc.ec2_ipam_pool.AwsService.global_services` |

Shorthand formats: `ec2` or `AwsService.ec2`

### ipam_scope_type (IpamScopeType)

| Value | DSL Identifier |
|-------|----------------|
| `public` | `awscc.ec2_ipam_pool.IpamScopeType.public` |
| `private` | `awscc.ec2_ipam_pool.IpamScopeType.private` |

Shorthand formats: `public` or `IpamScopeType.public`

### public_ip_source (PublicIpSource)

| Value | DSL Identifier |
|-------|----------------|
| `byoip` | `awscc.ec2_ipam_pool.PublicIpSource.byoip` |
| `amazon` | `awscc.ec2_ipam_pool.PublicIpSource.amazon` |

Shorthand formats: `byoip` or `PublicIpSource.byoip`

### state (State)

| Value | DSL Identifier |
|-------|----------------|
| `create-in-progress` | `awscc.ec2_ipam_pool.State.create_in_progress` |
| `create-complete` | `awscc.ec2_ipam_pool.State.create_complete` |
| `modify-in-progress` | `awscc.ec2_ipam_pool.State.modify_in_progress` |
| `modify-complete` | `awscc.ec2_ipam_pool.State.modify_complete` |
| `delete-in-progress` | `awscc.ec2_ipam_pool.State.delete_in_progress` |
| `delete-complete` | `awscc.ec2_ipam_pool.State.delete_complete` |

Shorthand formats: `create_in_progress` or `State.create_in_progress`

## Struct Definitions

### ProvisionedCidr

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr` | Ipv4Cidr | Yes |  |

### SourceResource

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `resource_id` | String | Yes |  |
| `resource_owner` | String | Yes |  |
| `resource_region` | String | Yes |  |
| `resource_type` | String | Yes |  |

## Attribute Reference

### `arn`

- **Type:** Arn

### `ipam_arn`

- **Type:** Arn

### `ipam_pool_id`

- **Type:** IpamPoolId

### `ipam_scope_arn`

- **Type:** Arn

### `ipam_scope_type`

- **Type:** [Enum (IpamScopeType)](#ipam_scope_type-ipamscopetype)

### `pool_depth`

- **Type:** Int

### `state`

- **Type:** [Enum (State)](#state-state)

### `state_message`

- **Type:** String



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
}

let ipam_pool = awscc.ec2_ipam_pool {
  ipam_scope_id  = ipam.private_default_scope_id
  address_family = "IPv4"
  locale         = "ap-northeast-1"
  description    = "Example IPv4 IPAM Pool"

  provisioned_cidrs = [
    {
      cidr = "10.0.0.0/8"
    }
  ]

  tags = {
    Environment = "example"
  }
}
```
