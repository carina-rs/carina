# awscc.ec2_security_group

CloudFormation Type: `AWS::EC2::SecurityGroup`

Resource Type definition for AWS::EC2::SecurityGroup

## Attributes

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `group_description` | String | Yes | A description for the security group. |
| `group_id` | String |  | (read-only) |
| `group_name` | String | No | The name of the security group. |
| `id` | String |  | (read-only) |
| `security_group_egress` | List | No | [VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group. |
| `security_group_ingress` | List | No | The inbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group. |
| `tags` | Map | No | Any tags assigned to the security group. |
| `vpc_id` | String | No | The ID of the VPC for the security group. |



## Example

```crn
let vpc = awscc.ec2_vpc {
  name       = "example-vpc"
  cidr_block = "10.0.0.0/16"
}

awscc.ec2_security_group {
  name              = "example-sg"
  vpc_id            = vpc.vpc_id
  group_description = "Example security group"

  tags = {
    Environment = "example"
  }
}
```
