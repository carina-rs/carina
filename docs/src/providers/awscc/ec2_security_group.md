# awscc.ec2_security_group

CloudFormation Type: `AWS::EC2::SecurityGroup`

Resource Type definition for AWS::EC2::SecurityGroup

## Argument Reference

### `group_description`

- **Type:** String
- **Required:** Yes

A description for the security group.

### `group_name`

- **Type:** String
- **Required:** No

The name of the security group.

### `security_group_egress`

- **Type:** List<Egress>
- **Required:** No

[VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.

### `security_group_ingress`

- **Type:** List<Ingress>
- **Required:** No

The inbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the security group.

### `vpc_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the VPC for the security group.

## Attribute Reference

### `group_id`

- **Type:** AwsResourceId

### `id`

- **Type:** String

## Struct Definitions

### Egress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr_ip` | Ipv4Cidr | No |  |
| `cidr_ipv6` | Ipv6Cidr | No |  |
| `description` | String | No |  |
| `destination_prefix_list_id` | AwsResourceId | No |  |
| `destination_security_group_id` | AwsResourceId | No |  |
| `from_port` | Int | No |  |
| `ip_protocol` | Enum | Yes |  |
| `to_port` | Int | No |  |

### Ingress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr_ip` | Ipv4Cidr | No |  |
| `cidr_ipv6` | Ipv6Cidr | No |  |
| `description` | String | No |  |
| `from_port` | Int | No |  |
| `ip_protocol` | Enum | Yes |  |
| `source_prefix_list_id` | AwsResourceId | No |  |
| `source_security_group_id` | AwsResourceId | No |  |
| `source_security_group_name` | String | No |  |
| `source_security_group_owner_id` | String | No |  |
| `to_port` | Int | No |  |



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

  security_group_ingress = [
    {
      ip_protocol = "tcp"
      from_port   = 80
      to_port     = 80
      cidr_ip     = "0.0.0.0/0"
    },
    {
      ip_protocol = "tcp"
      from_port   = 443
      to_port     = 443
      cidr_ip     = "0.0.0.0/0"
    }
  ]

  tags = {
    Environment = "example"
  }
}
```
