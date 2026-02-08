# awscc.ec2_route_table

CloudFormation Type: `AWS::EC2::RouteTable`

Specifies a route table for the specified VPC. After you create a route table, you can add routes and associate the table with a subnet.
 For more information, see [Route tables](https://docs.aws.amazon.com/vpc/latest/userguide/VPC_Route_Tables.html) in the *Amazon VPC User Guide*.

## Attributes

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `route_table_id` | String |  | (read-only) |
| `tags` | Map | No | Any tags assigned to the route table. |
| `vpc_id` | String | Yes | The ID of the VPC. |



## Example

```crn
let vpc = awscc.ec2_vpc {
  name                 = "example-vpc"
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

awscc.ec2_route_table {
  name   = "example-public-rt"
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}
```
