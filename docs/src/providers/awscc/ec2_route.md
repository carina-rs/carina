# awscc.ec2_route

CloudFormation Type: `AWS::EC2::Route`

Specifies a route in a route table. For more information, see [Routes](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Route_Tables.html#route-table-routes) in the *Amazon VPC User Guide*.
 You must specify either a destination CIDR block or prefix list ID. You must also specify exactly one of the resources as the target.
 If you create a route that references a transit gateway in the same template where you create the transit gateway, you must declare a dependency on the transit gateway attachment. The route table cannot use the transit gateway until it has successfully attached to the VPC. Add a [DependsOn Attribute](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-attribute-dependson.html) in the ``AWS::EC2::Route`` resource to explicitly declare a dependency on the ``AWS::EC2::TransitGatewayAttachment`` resource.

## Argument Reference

### `carrier_gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the carrier gateway. You can only use this option when the VPC contains a subnet which is associated with a Wavelength Zone.

### `core_network_arn`

- **Type:** Arn
- **Required:** No

The Amazon Resource Name (ARN) of the core network.

### `destination_cidr_block`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 CIDR address block used for the destination match. Routing decisions are based on the most specific match. We modify the specified CIDR block to its canonical form; for example, if you specify ``100.68.0.18/18``, we modify it to ``100.68.0.0/18``.

### `destination_ipv6_cidr_block`

- **Type:** Ipv6Cidr
- **Required:** No

The IPv6 CIDR block used for the destination match. Routing decisions are based on the most specific match.

### `destination_prefix_list_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of a prefix list used for the destination match.

### `egress_only_internet_gateway_id`

- **Type:** InternetGatewayId
- **Required:** No

[IPv6 traffic only] The ID of an egress-only internet gateway.

### `gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of an internet gateway or virtual private gateway attached to your VPC.

### `instance_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of a NAT instance in your VPC. The operation fails if you specify an instance ID unless exactly one network interface is attached.

### `local_gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the local gateway.

### `nat_gateway_id`

- **Type:** NatGatewayId
- **Required:** No

[IPv4 traffic only] The ID of a NAT gateway.

### `network_interface_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of a network interface.

### `route_table_id`

- **Type:** RouteTableId
- **Required:** Yes

The ID of the route table for the route.

### `transit_gateway_id`

- **Type:** TransitGatewayId
- **Required:** No

The ID of a transit gateway.

### `vpc_endpoint_id`

- **Type:** VpcEndpointId
- **Required:** No

The ID of a VPC endpoint. Supported for Gateway Load Balancer endpoints only.

### `vpc_peering_connection_id`

- **Type:** VpcPeeringConnectionId
- **Required:** No

The ID of a VPC peering connection.

## Attribute Reference

### `cidr_block`

- **Type:** Ipv4Cidr



## Example

```crn
let vpc = awscc.ec2_vpc {
  name                 = "example-vpc"
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let igw = awscc.ec2_internet_gateway {
  name = "example-igw"
}

let igw_attachment = awscc.ec2_vpc_gateway_attachment {
  name                = "example-igw-attachment"
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}

let rt = awscc.ec2_route_table {
  name   = "example-public-rt"
  vpc_id = vpc.vpc_id
}

awscc.ec2_route {
  name                   = "example-internet-route"
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw_attachment.internet_gateway_id
}
```
