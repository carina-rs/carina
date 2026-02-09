# awscc.ec2_security_group

CloudFormation Type: `AWS::EC2::SecurityGroup`

Resource Type definition for AWS::EC2::SecurityGroup

## Attributes

### `group_description`

- **Type:** String
- **Required:** Yes

A description for the security group.

### `group_id`

- **Type:** String
- **Read-only**

### `group_name`

- **Type:** String
- **Required:** No

The name of the security group.

### `id`

- **Type:** String
- **Read-only**

### `security_group_egress`

- **Type:** List
- **Required:** No

[VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.

### `security_group_ingress`

- **Type:** List
- **Required:** No

The inbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the security group.

### `vpc_id`

- **Type:** String
- **Required:** No

The ID of the VPC for the security group.



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
