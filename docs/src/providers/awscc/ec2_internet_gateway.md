# awscc.ec2.internet_gateway

CloudFormation Type: `AWS::EC2::InternetGateway`

Allocates an internet gateway for use with a VPC. After creating the Internet gateway, you then attach it to a VPC.

## Argument Reference

### `tags`

- **Type:** Map
- **Required:** No

Any tags to assign to the internet gateway.

## Attribute Reference

### `internet_gateway_id`

- **Type:** InternetGatewayId



## Example

```crn
let igw = awscc.ec2.internet_gateway {
  tags = {
    Environment = "example"
  }
}
```
