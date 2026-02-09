# awscc.ec2_subnet

CloudFormation Type: `AWS::EC2::Subnet`

Specifies a subnet for the specified VPC.
 For an IPv4 only subnet, specify an IPv4 CIDR block. If the VPC has an IPv6 CIDR block, you can create an IPv6 only subnet or a dual stack subnet instead. For an IPv6 only subnet, specify an IPv6 CIDR block. For a dual stack subnet, specify both an IPv4 CIDR block and an IPv6 CIDR block.
 For more information, see [Subnets for your VPC](https://docs.aws.amazon.com/vpc/latest/userguide/configure-subnets.html) in the *Amazon VPC User Guide*.

## Attributes

### `assign_ipv6_address_on_creation`

- **Type:** Bool
- **Required:** No

Indicates whether a network interface created in this subnet receives an IPv6 address. The default value is ``false``. If you specify ``AssignIpv6AddressOnCreation``, you must also specify an IPv6 CIDR block.

### `availability_zone`

- **Type:** String
- **Required:** No

The Availability Zone of the subnet. If you update this property, you must also update the ``CidrBlock`` property.

### `availability_zone_id`

- **Type:** String
- **Required:** No

The AZ ID of the subnet.

### `block_public_access_states`

- **Type:** Struct(BlockPublicAccessStates)
- **Read-only**

### `cidr_block`

- **Type:** CIDR
- **Required:** No

The IPv4 CIDR block assigned to the subnet. If you update this property, we create a new subnet, and then delete the existing one.

### `enable_dns64`

- **Type:** Bool
- **Required:** No

Indicates whether DNS queries made to the Amazon-provided DNS Resolver in this subnet should return synthetic IPv6 addresses for IPv4-only destinations.  You must first configure a NAT gateway in a public subnet (separate from the subnet containing the IPv6-only workloads). For example, the subnet containing the NAT gateway should have a ``0.0.0.0/0`` route pointing to the internet gateway. For more information, see [Configure DNS64 and NAT64](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-nat64-dns64.html#nat-gateway-nat64-dns64-walkthrough) in the *User Guide*.

### `enable_lni_at_device_index`

- **Type:** Int
- **Required:** No

Indicates the device position for local network interfaces in this subnet. For example, ``1`` indicates local network interfaces in this subnet are the secondary network interface (eth1).

### `ipv4_ipam_pool_id`

- **Type:** String
- **Required:** No

An IPv4 IPAM pool ID for the subnet.

### `ipv4_netmask_length`

- **Type:** Int
- **Required:** No

An IPv4 netmask length for the subnet.

### `ipv6_cidr_block`

- **Type:** CIDR
- **Required:** No

The IPv6 CIDR block. If you specify ``AssignIpv6AddressOnCreation``, you must also specify an IPv6 CIDR block.

### `ipv6_cidr_blocks`

- **Type:** List
- **Read-only**

### `ipv6_ipam_pool_id`

- **Type:** String
- **Required:** No

An IPv6 IPAM pool ID for the subnet.

### `ipv6_native`

- **Type:** Bool
- **Required:** No

Indicates whether this is an IPv6 only subnet. For more information, see [Subnet basics](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Subnets.html#subnet-basics) in the *User Guide*.

### `ipv6_netmask_length`

- **Type:** Int
- **Required:** No

An IPv6 netmask length for the subnet.

### `map_public_ip_on_launch`

- **Type:** Bool
- **Required:** No

Indicates whether instances launched in this subnet receive a public IPv4 address. The default value is ``false``. AWS charges for all public IPv4 addresses, including public IPv4 addresses associated with running instances and Elastic IP addresses. For more information, see the *Public IPv4 Address* tab on the [VPC pricing page](https://docs.aws.amazon.com/vpc/pricing/).

### `network_acl_association_id`

- **Type:** String
- **Read-only**

### `outpost_arn`

- **Type:** String
- **Required:** No

The Amazon Resource Name (ARN) of the Outpost.

### `private_dns_name_options_on_launch`

- **Type:** Struct(PrivateDnsNameOptionsOnLaunch)
- **Required:** No

The hostname type for EC2 instances launched into this subnet and how DNS A and AAAA record queries to the instances should be handled. For more information, see [Amazon EC2 instance hostname types](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-instance-naming.html) in the *User Guide*. Available options:  + EnableResourceNameDnsAAAARecord (true | false)  + EnableResourceNameDnsARecord (true | false)  + HostnameType (ip-name | resource-name)

### `subnet_id`

- **Type:** String
- **Read-only**

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the subnet.

### `vpc_id`

- **Type:** String
- **Required:** Yes

The ID of the VPC the subnet is in. If you update this property, you must also update the ``CidrBlock`` property.

## Struct Definitions

### BlockPublicAccessStates

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `internet_gateway_block_mode` | String | No | The mode of VPC BPA. Options here are off, block-bidirectional, block-ingress  |

### PrivateDnsNameOptionsOnLaunch

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `enable_resource_name_dns_aaaa_record` | Bool | No |  |
| `enable_resource_name_dns_a_record` | Bool | No |  |
| `hostname_type` | String | No |  |



## Example

```crn
let vpc = awscc.ec2_vpc {
  name                 = "example-vpc"
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

awscc.ec2_subnet {
  name                    = "example-public-subnet"
  vpc_id                  = vpc.vpc_id
  cidr_block              = "10.0.1.0/24"
  availability_zone       = "ap-northeast-1a"
  map_public_ip_on_launch = true

  tags = {
    Environment = "example"
  }
}
```
