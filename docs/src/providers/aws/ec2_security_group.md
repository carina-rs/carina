# aws.ec2_security_group

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

- **Type:** [List\<Egress\>](#egress)
- **Required:** No

[VPC only] The outbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.

### `security_group_ingress`

- **Type:** [List\<Ingress\>](#ingress)
- **Required:** No

The inbound rules associated with the security group. There is a short interruption during which you cannot connect to the security group.

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the security group.

### `vpc_id`

- **Type:** VpcId
- **Required:** No

The ID of the VPC for the security group.

## Struct Definitions

### Egress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr_ip` | Ipv4Cidr | No |  |
| `cidr_ipv6` | Ipv6Cidr | No |  |
| `description` | String | No |  |
| `destination_prefix_list_id` | AwsResourceId | No |  |
| `destination_security_group_id` | SecurityGroupId | No |  |
| `from_port` | Int(-1..=65535) | No |  |
| `ip_protocol` | Enum | Yes |  |
| `to_port` | Int(-1..=65535) | No |  |

### Ingress

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cidr_ip` | Ipv4Cidr | No |  |
| `cidr_ipv6` | Ipv6Cidr | No |  |
| `description` | String | No |  |
| `from_port` | Int(-1..=65535) | No |  |
| `ip_protocol` | Enum | Yes |  |
| `source_prefix_list_id` | AwsResourceId | No |  |
| `source_security_group_id` | SecurityGroupId | No |  |
| `source_security_group_name` | String | No |  |
| `source_security_group_owner_id` | String | No |  |
| `to_port` | Int(-1..=65535) | No |  |

## Attribute Reference

### `group_id`

- **Type:** SecurityGroupId

### `id`

- **Type:** String


