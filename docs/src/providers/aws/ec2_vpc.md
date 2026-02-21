# aws.ec2_vpc

CloudFormation Type: `AWS::EC2::VPC`

Describes a VPC.

## Argument Reference

### `cidr_block`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 network range for the VPC, in CIDR notation. For example, 		    10.0.0.0/16. We modify the specified CIDR block to its canonical form; for example, if you specify 100.68.0.18/18, we modify it to 100.68.0.0/18.

### `instance_tenancy`

- **Type:** [Enum (InstanceTenancy)](#instance_tenancy-instancetenancy)
- **Required:** No

The tenancy options for instances launched into the VPC. For default, instances    are launched with shared tenancy by default. You can launch instances with any tenancy into a    shared tenancy VPC. For dedicated, instances are launched as dedicated tenancy    instances by default. You can only launch instances with a tenancy of dedicated    or host into a dedicated tenancy VPC.            Important: The host value cannot be used with this parameter. Use the default or dedicated values only.     Default: default

### `ipv4_ipam_pool_id`

- **Type:** IpamPoolId
- **Required:** No

The ID of an IPv4 IPAM pool you want to use for allocating this VPC's CIDR. For more information, see What is IPAM? in the Amazon VPC IPAM User Guide.

### `ipv4_netmask_length`

- **Type:** Int(0..=32)
- **Required:** No

The netmask length of the IPv4 CIDR you want to allocate to this VPC from an Amazon VPC IP Address Manager (IPAM) pool. For more information about IPAM, see What is IPAM? in the Amazon VPC IPAM User Guide.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### instance_tenancy (InstanceTenancy)

| Value | DSL Identifier |
|-------|----------------|
| `dedicated` | `aws.ec2_vpc.InstanceTenancy.dedicated` |
| `default` | `aws.ec2_vpc.InstanceTenancy.default` |
| `host` | `aws.ec2_vpc.InstanceTenancy.host` |

Shorthand formats: `dedicated` or `InstanceTenancy.dedicated`

## Attribute Reference

### `vpc_id`

- **Type:** VpcId

