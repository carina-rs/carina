# aws.ec2_security_group_egress

CloudFormation Type: `AWS::EC2::SecurityGroupEgress`

Describes a security group rule.

## Argument Reference

### `cidr_ip`

- **Type:** Ipv4Cidr
- **Required:** No

Not supported. Use IP permissions instead.

### `from_port`

- **Type:** Int(-1..=65535)
- **Required:** No

Not supported. Use IP permissions instead.

### `group_id`

- **Type:** SecurityGroupId
- **Required:** Yes

The ID of the security group.

### `ip_protocol`

- **Type:** [Enum (IpProtocol)](#ip_protocol-ipprotocol)
- **Required:** Yes

Not supported. Use IP permissions instead.

### `source_security_group_name`

- **Type:** String
- **Required:** No

Not supported. Use IP permissions instead.

### `source_security_group_owner_id`

- **Type:** String
- **Required:** No

Not supported. Use IP permissions instead.

### `to_port`

- **Type:** Int(-1..=65535)
- **Required:** No

Not supported. Use IP permissions instead.

## Enum Values

### ip_protocol (IpProtocol)

| Value | DSL Identifier |
|-------|----------------|
| `tcp` | `aws.ec2_security_group_egress.IpProtocol.tcp` |
| `udp` | `aws.ec2_security_group_egress.IpProtocol.udp` |
| `icmp` | `aws.ec2_security_group_egress.IpProtocol.icmp` |
| `icmpv6` | `aws.ec2_security_group_egress.IpProtocol.icmpv6` |
| `-1` | `aws.ec2_security_group_egress.IpProtocol.all` |

Shorthand formats: `tcp` or `IpProtocol.tcp`

## Attribute Reference

### `security_group_rule_id`

- **Type:** String

