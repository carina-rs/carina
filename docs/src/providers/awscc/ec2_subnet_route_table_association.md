# awscc.ec2_subnet_route_table_association

CloudFormation Type: `AWS::EC2::SubnetRouteTableAssociation`

Associates a subnet with a route table. The subnet and route table must be in the same VPC. This association causes traffic originating from the subnet to be routed according to the routes in the route table. A route table can be associated with multiple subnets. To create a route table, see [AWS::EC2::RouteTable](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-routetable.html).

## Argument Reference

### `route_table_id`

- **Type:** RouteTableId
- **Required:** Yes

The ID of the route table. The physical ID changes when the route table ID is changed.

### `subnet_id`

- **Type:** SubnetId
- **Required:** Yes

The ID of the subnet.

## Attribute Reference

### `id`

- **Type:** String



## Example

```crn
let vpc = awscc.ec2_vpc {
  name                 = "example-vpc"
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let subnet = awscc.ec2_subnet {
  name              = "example-subnet"
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = "ap-northeast-1a"
}

let rt = awscc.ec2_route_table {
  name   = "example-rt"
  vpc_id = vpc.vpc_id
}

awscc.ec2_subnet_route_table_association {
  name           = "example-subnet-rt-assoc"
  subnet_id      = subnet.subnet_id
  route_table_id = rt.route_table_id
}
```
