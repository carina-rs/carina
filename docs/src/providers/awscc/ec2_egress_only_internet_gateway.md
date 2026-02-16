# awscc.ec2_egress_only_internet_gateway

CloudFormation Type: `AWS::EC2::EgressOnlyInternetGateway`

Resource Type definition for AWS::EC2::EgressOnlyInternetGateway

## Argument Reference

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the egress only internet gateway.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC for which to create the egress-only internet gateway.

## Attribute Reference

### `id`

- **Type:** String



## Example

```crn
let vpc = awscc.ec2_vpc {
  cidr_block = "10.0.0.0/16"
}

let eoigw = awscc.ec2_egress_only_internet_gateway {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}
```
