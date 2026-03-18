# aws.ec2.route

CloudFormation Type: `AWS::EC2::Route`

Describes a route in a route table.

## Example

```crn
let vpc = aws.ec2.vpc {
  cidr_block = "10.0.0.0/16"

  tags = {
    Environment = "example"
  }
}

let igw = aws.ec2.internet_gateway {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}

let rt = aws.ec2.route_table {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}

let route = aws.ec2.route {
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw.internet_gateway_id
}
```

## Argument Reference

### `destination_cidr_block`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 CIDR address block used for the destination match. Routing decisions are based on the most specific match. We modify the specified CIDR block to its canonical form; for example, if you specify 100.68.0.18/18, we modify it to 100.68.0.0/18.

### `gateway_id`

- **Type:** GatewayId
- **Required:** No

The ID of an internet gateway or virtual private gateway attached to your VPC.

### `nat_gateway_id`

- **Type:** nat_gateway_id
- **Required:** No

[IPv4 traffic only] The ID of a NAT gateway.

### `route_table_id`

- **Type:** route_table_id
- **Required:** Yes

The ID of the route table for the route.

