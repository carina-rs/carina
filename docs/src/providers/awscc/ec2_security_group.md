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

- **Type:** String
- **Required:** No

The ID of the VPC for the security group.

## Struct Definitions

### Egress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr_ip` | Ipv4Cidr | No |  |
| `cidr_ipv6` | Ipv6Cidr | No |  |
| `description` | String | No |  |
| `destination_prefix_list_id` | String | No |  |
| `destination_security_group_id` | String | No |  |
| `from_port` | Int | No |  |
| `ip_protocol` | String | Yes |  |
| `to_port` | Int | No |  |

### Ingress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr_ip` | Ipv4Cidr | No |  |
| `cidr_ipv6` | Ipv6Cidr | No |  |
| `description` | String | No |  |
| `from_port` | Int | No |  |
| `ip_protocol` | String | Yes |  |
| `source_prefix_list_id` | String | No |  |
| `source_security_group_id` | String | No |  |
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

  tags = {
    Environment = "example"
  }
}
```
