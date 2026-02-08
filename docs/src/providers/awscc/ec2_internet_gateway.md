# awscc.ec2_internet_gateway

CloudFormation Type: `AWS::EC2::InternetGateway`

Allocates an internet gateway for use with a VPC. After creating the Internet gateway, you then attach it to a VPC.

## Attributes

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `internet_gateway_id` | String |  | (read-only) |
| `tags` | Map | No | Any tags to assign to the internet gateway. |



## Example

```crn
awscc.ec2_internet_gateway {
  name = "example-igw"

  tags = {
    Environment = "example"
  }
}
```
