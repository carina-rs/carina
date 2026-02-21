# aws.ec2.subnet

CloudFormation Type: `AWS::EC2::Subnet`

Describes a subnet.

## Argument Reference

### `assign_ipv6_address_on_creation`

- **Type:** Bool
- **Required:** No

Indicates whether a network interface created in this subnet (including a network       interface created by RunInstances) receives an IPv6 address.

### `availability_zone`

- **Type:** AvailabilityZone
- **Required:** No

The Availability Zone or Local Zone for the subnet.     Default: Amazon Web Services selects one for you. If you create more than one subnet in your VPC, we      do not necessarily select a different zone for each subnet.     To create a subnet in a Local Zone, set this value to the Local Zone ID, for example      us-west-2-lax-1a. For information about the Regions that support Local Zones,       see Available Local Zones.     To create a subnet in an Outpost, set this value to the Availability Zone for the      Outpost and specify the Outpost ARN.

### `availability_zone_id`

- **Type:** String
- **Required:** No

The AZ ID or the Local Zone ID of the subnet.

### `cidr_block`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 network range for the subnet, in CIDR notation. For example, 10.0.0.0/24.       We modify the specified CIDR block to its canonical form; for example, if you specify       100.68.0.18/18, we modify it to 100.68.0.0/18.     This parameter is not supported for an IPv6 only subnet.

### `enable_dns64`

- **Type:** Bool
- **Required:** No

Indicates whether DNS queries made to the Amazon-provided DNS Resolver in this subnet       should return synthetic IPv6 addresses for IPv4-only destinations.

### `enable_lni_at_device_index`

- **Type:** Int
- **Required:** No

Indicates the device position for local network interfaces in this subnet. For example,       1 indicates local network interfaces in this subnet are the secondary       network interface (eth1).

### `ipv4_ipam_pool_id`

- **Type:** IpamPoolId
- **Required:** No

An IPv4 IPAM pool ID for the subnet.

### `ipv4_netmask_length`

- **Type:** Int(0..=32)
- **Required:** No

An IPv4 netmask length for the subnet.

### `ipv6_cidr_block`

- **Type:** Ipv6Cidr
- **Required:** No

The IPv6 network range for the subnet, in CIDR notation. This parameter is required       for an IPv6 only subnet.

### `ipv6_ipam_pool_id`

- **Type:** IpamPoolId
- **Required:** No

An IPv6 IPAM pool ID for the subnet.

### `ipv6_native`

- **Type:** Bool
- **Required:** No

Indicates whether to create an IPv6 only subnet.

### `ipv6_netmask_length`

- **Type:** Int(0..=128)
- **Required:** No

An IPv6 netmask length for the subnet.

### `map_public_ip_on_launch`

- **Type:** Bool
- **Required:** No

Indicates whether instances launched in this subnet receive a public IPv4 address.     Amazon Web Services charges for all public IPv4 addresses, including public IPv4 addresses associated with running instances and Elastic IP addresses. For more information, see the Public IPv4 Address tab on the Amazon VPC pricing page.

### `outpost_arn`

- **Type:** Arn
- **Required:** No

The Amazon Resource Name (ARN) of the Outpost. If you specify an Outpost ARN, you must also     specify the Availability Zone of the Outpost subnet.

### `private_dns_name_options_on_launch`

- **Type:** [Struct(PrivateDnsNameOptionsOnLaunch)](#privatednsnameoptionsonlaunch)
- **Required:** No

The type of hostnames to assign to instances in the subnet at launch. An instance hostname       is based on the IPv4 address or ID of the instance.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Struct Definitions

### PrivateDnsNameOptionsOnLaunch

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `enable_resource_name_dns_aaaa_record` | Bool | No | Indicates whether to respond to DNS queries for instance hostname with DNS AAAA       records. |
| `enable_resource_name_dns_a_record` | Bool | No | Indicates whether to respond to DNS queries for instance hostnames with DNS A       records. |
| `hostname_type` | [Enum (HostnameType)](#hostname_type-hostnametype) | No | The type of hostname for EC2 instances. For IPv4 only subnets, an instance DNS name       must be ba... |

## Attribute Reference

### `subnet_id`

- **Type:** SubnetId

