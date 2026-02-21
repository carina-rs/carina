# aws.ec2.route

CloudFormation Type: `AWS::EC2::Route`

Describes a route in a route table.

## Argument Reference

### `carrier_gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the carrier gateway.     You can only use this option when the VPC contains a subnet which is associated with a Wavelength Zone.

### `core_network_arn`

- **Type:** Arn
- **Required:** No

The Amazon Resource Name (ARN) of the core network.

### `destination_cidr_block`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 CIDR address block used for the destination match. Routing decisions are based on the most specific match. We modify the specified CIDR block to its canonical form; for example, if you specify 100.68.0.18/18, we modify it to 100.68.0.0/18.

### `destination_ipv6_cidr_block`

- **Type:** Ipv6Cidr
- **Required:** No

The IPv6 CIDR block used for the destination match. Routing decisions are based on the most specific match.

### `destination_prefix_list_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of a prefix list used for the destination match.

### `egress_only_internet_gateway_id`

- **Type:** egress_only_internet_gateway_id
- **Required:** No

[IPv6 traffic only] The ID of an egress-only internet gateway.

### `gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of an internet gateway or virtual private gateway attached to your 			VPC.

### `instance_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of a NAT instance in your VPC. The operation fails if you specify an instance ID unless exactly one network interface is attached.

### `local_gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the local gateway.

### `nat_gateway_id`

- **Type:** nat_gateway_id
- **Required:** No

[IPv4 traffic only] The ID of a NAT gateway.

### `network_interface_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of a network interface.

### `transit_gateway_id`

- **Type:** transit_gateway_id
- **Required:** No

The ID of a transit gateway.

### `vpc_endpoint_id`

- **Type:** vpc_endpoint_id
- **Required:** No

The ID of a VPC endpoint. Supported for Gateway Load Balancer endpoints only.

### `vpc_peering_connection_id`

- **Type:** vpc_peering_connection_id
- **Required:** No

The ID of a VPC peering connection.

