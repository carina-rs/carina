# awscc.ec2.nat_gateway

CloudFormation Type: `AWS::EC2::NatGateway`

Specifies a network address translation (NAT) gateway in the specified subnet. You can create either a public NAT gateway or a private NAT gateway. The default is a public NAT gateway. If you create a public NAT gateway, you must specify an elastic IP address.
 With a NAT gateway, instances in a private subnet can connect to the internet, other AWS services, or an on-premises network using the IP address of the NAT gateway. For more information, see [NAT gateways](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-nat-gateway.html) in the *Amazon VPC User Guide*.
 If you add a default route (``AWS::EC2::Route`` resource) that points to a NAT gateway, specify the NAT gateway ID for the route's ``NatGatewayId`` property.
  When you associate an Elastic IP address or secondary Elastic IP address with a public NAT gateway, the network border group of the Elastic IP address must match the network border group of the Availability Zone (AZ) that the public NAT gateway is in. Otherwise, the NAT gateway fails to launch. You can see the network border group for the AZ by viewing the details of the subnet. Similarly, you can view the network border group for the Elastic IP address by viewing its details. For more information, see [Allocate an Elastic IP address](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-eips.html#allocate-eip) in the *Amazon VPC User Guide*.

## Argument Reference

### `allocation_id`

- **Type:** AwsResourceId
- **Required:** No

[Public NAT gateway only] The allocation ID of the Elastic IP address that's associated with the NAT gateway. This property is required for a public NAT gateway and cannot be specified with a private NAT gateway.

### `availability_mode`

- **Type:** [Enum (AvailabilityMode)](#availability_mode-availabilitymode)
- **Required:** No

Indicates whether this is a zonal (single-AZ) or regional (multi-AZ) NAT gateway. A zonal NAT gateway is a NAT Gateway that provides redundancy and scalability within a single availability zone. A regional NAT gateway is a single NAT Gateway that works across multiple availability zones (AZs) in your VPC, providing redundancy, scalability and availability across all the AZs in a Region. For more information, see [Regional NAT gateways for automatic multi-AZ expansion](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateways-regional.html) in the *Amazon VPC User Guide*.

### `availability_zone_addresses`

- **Type:** [List\<AvailabilityZoneAddress\>](#availabilityzoneaddress)
- **Required:** No

For regional NAT gateways only: Specifies which Availability Zones you want the NAT gateway to support and the Elastic IP addresses (EIPs) to use in each AZ. The regional NAT gateway uses these EIPs to handle outbound NAT traffic from their respective AZs. If not specified, the NAT gateway will automatically expand to new AZs and associate EIPs upon detection of an elastic network interface. If you specify this parameter, auto-expansion is disabled and you must manually manage AZ coverage. A regional NAT gateway is a single NAT Gateway that works across multiple availability zones (AZs) in your VPC, providing redundancy, scalability and availability across all the AZs in a Region. For more information, see [Regional NAT gateways for automatic multi-AZ expansion](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateways-regional.html) in the *Amazon VPC User Guide*.

### `connectivity_type`

- **Type:** [Enum (ConnectivityType)](#connectivity_type-connectivitytype)
- **Required:** No

Indicates whether the NAT gateway supports public or private connectivity. The default is public connectivity.

### `max_drain_duration_seconds`

- **Type:** Int
- **Required:** No

The maximum amount of time to wait (in seconds) before forcibly releasing the IP addresses if connections are still in progress. Default value is 350 seconds.

### `private_ip_address`

- **Type:** Ipv4Address
- **Required:** No

The private IPv4 address to assign to the NAT gateway. If you don't provide an address, a private IPv4 address will be automatically assigned.

### `secondary_allocation_ids`

- **Type:** `List<AwsResourceId>`
- **Required:** No

Secondary EIP allocation IDs. For more information, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-with.html) in the *Amazon VPC User Guide*.

### `secondary_private_ip_address_count`

- **Type:** Int
- **Required:** No

[Private NAT gateway only] The number of secondary private IPv4 addresses you want to assign to the NAT gateway. For more information about secondary addresses, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-nat-gateway.html#nat-gateway-creating) in the *Amazon Virtual Private Cloud User Guide*. ``SecondaryPrivateIpAddressCount`` and ``SecondaryPrivateIpAddresses`` cannot be set at the same time.

### `secondary_private_ip_addresses`

- **Type:** `List<Ipv4Address>`
- **Required:** No

Secondary private IPv4 addresses. For more information about secondary addresses, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-nat-gateway.html#nat-gateway-creating) in the *Amazon Virtual Private Cloud User Guide*. ``SecondaryPrivateIpAddressCount`` and ``SecondaryPrivateIpAddresses`` cannot be set at the same time.

### `subnet_id`

- **Type:** SubnetId
- **Required:** No

The ID of the subnet in which the NAT gateway is located.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the NAT gateway.

### `vpc_id`

- **Type:** VpcId
- **Required:** No

The ID of the VPC in which the NAT gateway is located.

## Enum Values

### availability_mode (AvailabilityMode)

| Value | DSL Identifier |
|-------|----------------|
| `zonal` | `awscc.ec2.nat_gateway.AvailabilityMode.zonal` |
| `regional` | `awscc.ec2.nat_gateway.AvailabilityMode.regional` |

Shorthand formats: `zonal` or `AvailabilityMode.zonal`

### connectivity_type (ConnectivityType)

| Value | DSL Identifier |
|-------|----------------|
| `public` | `awscc.ec2.nat_gateway.ConnectivityType.public` |
| `private` | `awscc.ec2.nat_gateway.ConnectivityType.private` |

Shorthand formats: `public` or `ConnectivityType.public`

## Struct Definitions

### AvailabilityZoneAddress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `allocation_ids` | `List<AwsResourceId>` | Yes | The allocation IDs of the Elastic IP addresses (EIPs) to be used for handling outbound NAT traffic i... |
| `availability_zone` | AvailabilityZone | No | For regional NAT gateways only: The Availability Zone where this specific NAT gateway configuration ... |
| `availability_zone_id` | String | No | For regional NAT gateways only: The ID of the Availability Zone where this specific NAT gateway conf... |

## Attribute Reference

### `auto_provision_zones`

- **Type:** String

### `auto_scaling_ips`

- **Type:** String

### `eni_id`

- **Type:** AwsResourceId

### `nat_gateway_id`

- **Type:** NatGatewayId

### `route_table_id`

- **Type:** RouteTableId



## Example

```crn
let vpc = awscc.ec2.vpc {
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let public_subnet = awscc.ec2.subnet {
  vpc_id                  = vpc.vpc_id
  cidr_block              = "10.0.1.0/24"
  availability_zone       = "ap-northeast-1a"
  map_public_ip_on_launch = true
}

let eip = awscc.ec2.eip {
  domain = "vpc"
}

let nat_gw = awscc.ec2.nat_gateway {
  allocation_id = eip.allocation_id
  subnet_id     = public_subnet.subnet_id

  tags = {
    Environment = "example"
  }
}
```
