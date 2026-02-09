# awscc.ec2_vpc

CloudFormation Type: `AWS::EC2::VPC`

Specifies a virtual private cloud (VPC).
 To add an IPv6 CIDR block to the VPC, see [AWS::EC2::VPCCidrBlock](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpccidrblock.html).
 For more information, see [Virtual private clouds (VPC)](https://docs.aws.amazon.com/vpc/latest/userguide/configure-your-vpc.html) in the *Amazon VPC User Guide*.

## Attributes

### `cidr_block`

- **Type:** CIDR
- **Required:** No

The IPv4 network range for the VPC, in CIDR notation. For example, ``10.0.0.0/16``. We modify the specified CIDR block to its canonical form; for example, if you specify ``100.68.0.18/18``, we modify it to ``100.68.0.0/18``. You must specify either``CidrBlock`` or ``Ipv4IpamPoolId``.

### `cidr_block_associations`

- **Type:** List
- **Read-only**

### `default_network_acl`

- **Type:** String
- **Read-only**

### `default_security_group`

- **Type:** String
- **Read-only**

### `enable_dns_hostnames`

- **Type:** Bool
- **Required:** No

Indicates whether the instances launched in the VPC get DNS hostnames. If enabled, instances in the VPC get DNS hostnames; otherwise, they do not. Disabled by default for nondefault VPCs. For more information, see [DNS attributes in your VPC](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-dns.html#vpc-dns-support). You can only enable DNS hostnames if you've enabled DNS support.

### `enable_dns_support`

- **Type:** Bool
- **Required:** No

Indicates whether the DNS resolution is supported for the VPC. If enabled, queries to the Amazon provided DNS server at the 169.254.169.253 IP address, or the reserved IP address at the base of the VPC network range "plus two" succeed. If disabled, the Amazon provided DNS service in the VPC that resolves public DNS hostnames to IP addresses is not enabled. Enabled by default. For more information, see [DNS attributes in your VPC](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-dns.html#vpc-dns-support).

### `instance_tenancy`

- **Type:** Enum (InstanceTenancy)
- **Required:** No

The allowed tenancy of instances launched into the VPC.  + ``default``: An instance launched into the VPC runs on shared hardware by default, unless you explicitly specify a different tenancy during instance launch.  + ``dedicated``: An instance launched into the VPC runs on dedicated hardware by default, unless you explicitly specify a tenancy of ``host`` during instance launch. You cannot specify a tenancy of ``default`` during instance launch.   Updating ``InstanceTenancy`` requires no replacement only if you are updating its value from ``dedicated`` to ``default``. Updating ``InstanceTenancy`` from ``default`` to ``dedicated`` requires replacement.

### `ipv4_ipam_pool_id`

- **Type:** String
- **Required:** No

The ID of an IPv4 IPAM pool you want to use for allocating this VPC's CIDR. For more information, see [What is IPAM?](https://docs.aws.amazon.com//vpc/latest/ipam/what-is-it-ipam.html) in the *Amazon VPC IPAM User Guide*. You must specify either``CidrBlock`` or ``Ipv4IpamPoolId``.

### `ipv4_netmask_length`

- **Type:** Int
- **Required:** No

The netmask length of the IPv4 CIDR you want to allocate to this VPC from an Amazon VPC IP Address Manager (IPAM) pool. For more information about IPAM, see [What is IPAM?](https://docs.aws.amazon.com//vpc/latest/ipam/what-is-it-ipam.html) in the *Amazon VPC IPAM User Guide*.

### `ipv6_cidr_blocks`

- **Type:** List
- **Read-only**

### `tags`

- **Type:** Map
- **Required:** No

The tags for the VPC.

### `vpc_id`

- **Type:** String
- **Read-only**

## Enum Values

### instance_tenancy (InstanceTenancy)

| Value | DSL Identifier |
|-------|----------------|
| `default` | `awscc.ec2_vpc.InstanceTenancy.default` |
| `dedicated` | `awscc.ec2_vpc.InstanceTenancy.dedicated` |
| `host` | `awscc.ec2_vpc.InstanceTenancy.host` |

Shorthand formats: `default` or `InstanceTenancy.default`



## Example

```crn
awscc.ec2_vpc {
  name                 = "example-vpc"
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
  instance_tenancy     = default

  tags = {
    Environment = "example"
  }
}
```
