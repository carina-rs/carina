---
title: "aws.ec2.security_group_egress"
description: "AWS EC2 security_group_egress resource reference"
---

CloudFormation Type: `AWS::EC2::SecurityGroupEgress`

Describes a security group rule.

## Example

```crn
let vpc = aws.ec2.vpc {
  cidr_block = "10.0.0.0/16"

  tags = {
    Environment = "example"
  }
}

let sg = aws.ec2.security_group {
  group_name  = "carina-example-sg-egress"
  description = "SG for egress rule example"
  vpc_id      = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}

let egress = aws.ec2.security_group_egress {
  group_id    = sg.group_id
  description = "Allow HTTPS outbound"
  ip_protocol = tcp
  from_port   = 443
  to_port     = 443
  cidr_ip     = "0.0.0.0/0"
}
```

## Argument Reference

### `cidr_ip`

- **Type:** Ipv4Cidr
- **Required:** No

Not supported. Use IP permissions instead.

### `cidr_ipv6`

- **Type:** Ipv6Cidr
- **Required:** No

The IPv6 CIDR range.

### `description`

- **Type:** String
- **Required:** No

The security group rule description.

### `destination_prefix_list_id`

- **Type:** PrefixListId
- **Required:** No

The ID of the destination prefix list.

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

- **Type:** AwsAccountId
- **Required:** No

Not supported. Use IP permissions instead.

### `to_port`

- **Type:** Int(-1..=65535)
- **Required:** No

Not supported. Use IP permissions instead.

### `destination_security_group_id`

- **Type:** SecurityGroupId
- **Required:** No

The ID of the destination security group.

## Enum Values

### ip_protocol (IpProtocol)

| Value | DSL Identifier |
|-------|----------------|
| `tcp` | `aws.ec2.security_group_egress.IpProtocol.tcp` |
| `udp` | `aws.ec2.security_group_egress.IpProtocol.udp` |
| `icmp` | `aws.ec2.security_group_egress.IpProtocol.icmp` |
| `icmpv6` | `aws.ec2.security_group_egress.IpProtocol.icmpv6` |
| `-1` | `aws.ec2.security_group_egress.IpProtocol.all` |

Shorthand formats: `tcp` or `IpProtocol.tcp`

## Attribute Reference

### `security_group_rule_id`

- **Type:** SecurityGroupRuleId

