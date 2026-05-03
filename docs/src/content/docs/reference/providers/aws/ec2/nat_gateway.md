---
title: "aws.ec2.NatGateway"
description: "AWS EC2 NatGateway resource reference"
---


CloudFormation Type: `AWS::EC2::NatGateway`

Describes a NAT gateway.

## Argument Reference

### `allocation_id`

- **Type:** AllocationId
- **Required:** No

[Public NAT gateways only] The allocation ID of an Elastic IP address to associate with the NAT gateway. You cannot specify an Elastic IP address with a private NAT gateway. If the Elastic IP address is associated with another resource, you must first disassociate it.

### `availability_mode`

- **Type:** [Enum (AvailabilityMode)](#availability_mode-availabilitymode)
- **Required:** No

Specifies whether to create a zonal (single-AZ) or regional (multi-AZ) NAT gateway. Defaults to zonal. A zonal NAT gateway is a NAT Gateway that provides redundancy and scalability within a single availability zone. A regional NAT gateway is a single NAT Gateway that works across multiple availability zones (AZs) in your VPC, providing redundancy, scalability and availability across all the AZs in a Region. For more information, see Regional NAT gateways for automatic multi-AZ expansion in the Amazon VPC User Guide.

### `availability_zone_addresses`

- **Type:** `List<[Struct(AvailabilityZoneAddress)](#availabilityzoneaddress)>`
- **Required:** No

For regional NAT gateways only: Specifies which Availability Zones you want the NAT gateway to support and the Elastic IP addresses (EIPs) to use in each AZ. The regional NAT gateway uses these EIPs to handle outbound NAT traffic from their respective AZs. If not specified, the NAT gateway will automatically expand to new AZs and associate EIPs upon detection of an elastic network interface. If you specify this parameter, auto-expansion is disabled and you must manually manage AZ coverage. A regional NAT gateway is a single NAT Gateway that works across multiple availability zones (AZs) in your VPC, providing redundancy, scalability and availability across all the AZs in a Region. For more information, see Regional NAT gateways for automatic multi-AZ expansion in the Amazon VPC User Guide.

### `connectivity_type`

- **Type:** [Enum (ConnectivityType)](#connectivity_type-connectivitytype)
- **Required:** No

Indicates whether the NAT gateway supports public or private connectivity. The default is public connectivity.

### `private_ip_address`

- **Type:** Ipv4Address
- **Required:** No

The private IPv4 address to assign to the NAT gateway. If you don't provide an address, a private IPv4 address will be automatically assigned.

### `subnet_id`

- **Type:** SubnetId
- **Required:** Yes

The ID of the subnet in which to create the NAT gateway.

### `vpc_id`

- **Type:** VpcId
- **Required:** No

The ID of the VPC where you want to create a regional NAT gateway.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### availability_mode (AvailabilityMode)

| Value | DSL Identifier |
|-------|----------------|
| `regional` | `aws.ec2.NatGateway.AvailabilityMode.regional` |
| `zonal` | `aws.ec2.NatGateway.AvailabilityMode.zonal` |

Shorthand formats: `regional` or `AvailabilityMode.regional`

### connectivity_type (ConnectivityType)

| Value | DSL Identifier |
|-------|----------------|
| `private` | `aws.ec2.NatGateway.ConnectivityType.private` |
| `public` | `aws.ec2.NatGateway.ConnectivityType.public` |

Shorthand formats: `private` or `ConnectivityType.private`

## Struct Definitions

### AvailabilityZoneAddress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `allocation_ids` | `List<AllocationId>` | No | The allocation IDs of the Elastic IP addresses (EIPs) to be used for handling outbound NAT traffic in this specific Availability Zone. |
| `availability_zone` | AvailabilityZone | No | For regional NAT gateways only: The Availability Zone where this specific NAT gateway configuration will be active. Each AZ in a regional NAT gateway has its own configuration to handle outbound NAT traffic from that AZ. A regional NAT gateway is a single NAT Gateway that works across multiple availability zones (AZs) in your VPC, providing redundancy, scalability and availability across all the AZs in a Region. |
| `availability_zone_id` | AvailabilityZoneId | No | For regional NAT gateways only: The ID of the Availability Zone where this specific NAT gateway configuration will be active. Each AZ in a regional NAT gateway has its own configuration to handle outbound NAT traffic from that AZ. Use this instead of AvailabilityZone for consistent identification of AZs across Amazon Web Services Regions. A regional NAT gateway is a single NAT Gateway that works across multiple availability zones (AZs) in your VPC, providing redundancy, scalability and availability across all the AZs in a Region. |

## Attribute Reference

### `nat_gateway_id`

- **Type:** NatGatewayId

