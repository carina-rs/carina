# awscc.ec2.route_table

CloudFormation Type: `AWS::EC2::RouteTable`

Specifies a route table for the specified VPC. After you create a route table, you can add routes and associate the table with a subnet.
 For more information, see [Route tables](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Route_Tables.html) in the *Amazon VPC User Guide*.

## Argument Reference

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the route table.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC.

## Attribute Reference

### `route_table_id`

- **Type:** RouteTableId



## Example

```crn
let vpc = awscc.ec2.vpc {
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let rt = awscc.ec2.route_table {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}
```
