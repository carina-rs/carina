# aws.ec2_security_group_ingress

CloudFormation Type: `AWS::EC2::SecurityGroupIngress`

Describes a security group rule.

## Argument Reference

### `cidr_ip`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 address range, in CIDR format.                    Amazon Web Services canonicalizes IPv4 and IPv6 CIDRs. For example, if you specify 100.68.0.18/18 for the CIDR block,        Amazon Web Services canonicalizes the CIDR block to 100.68.0.0/18. Any subsequent DescribeSecurityGroups and DescribeSecurityGroupRules calls will        return the canonicalized form of the CIDR block. Additionally, if you attempt to add another rule with the        non-canonical form of the CIDR (such as 100.68.0.18/18) and there is already a rule for the canonicalized        form of the CIDR block (such as 100.68.0.0/18), the API throws an duplicate rule error.          To specify an IPv6 address range, use IP permissions instead.     To specify multiple rules and descriptions for the rules, use IP permissions instead.

### `from_port`

- **Type:** Int(-1..=65535)
- **Required:** No

If the protocol is TCP or UDP, this is the start of the port range.      If the protocol is ICMP, this is the ICMP type or -1 (all ICMP types).     To specify multiple rules and descriptions for the rules, use IP permissions instead.

### `group_id`

- **Type:** SecurityGroupId
- **Required:** No

The ID of the security group.

### `group_name`

- **Type:** String
- **Required:** No

[Default VPC] The name of the security group. For security groups for a default VPC     you can specify either the ID or the name of the security group. For security groups for     a nondefault VPC, you must specify the ID of the security group.

### `ip_protocol`

- **Type:** [Enum (IpProtocol)](#ip_protocol-ipprotocol)
- **Required:** Yes

The IP protocol name (tcp, udp, icmp) or number    (see Protocol Numbers). To specify all protocols, use -1.     To specify icmpv6, use IP permissions instead.     If you specify a protocol other than one of the supported values, traffic is allowed      on all ports, regardless of any ports that you specify.     To specify multiple rules and descriptions for the rules, use IP permissions instead.

### `source_security_group_name`

- **Type:** String
- **Required:** No

[Default VPC] The name of the source security group.     The rule grants full ICMP, UDP, and TCP access. To create a rule with a specific protocol       and port range, specify a set of IP permissions instead.

### `source_security_group_owner_id`

- **Type:** String
- **Required:** No

The Amazon Web Services account ID for the source security group, if the source security group is      in a different account.     The rule grants full ICMP, UDP, and TCP access. To create a rule with a specific protocol      and port range, use IP permissions instead.

### `to_port`

- **Type:** Int(-1..=65535)
- **Required:** No

If the protocol is TCP or UDP, this is the end of the port range.      If the protocol is ICMP, this is the ICMP code or -1 (all ICMP codes).       If the start port is -1 (all ICMP types), then the end port must be -1 (all ICMP codes).     To specify multiple rules and descriptions for the rules, use IP permissions instead.

## Enum Values

### ip_protocol (IpProtocol)

| Value | DSL Identifier |
|-------|----------------|
| `tcp` | `aws.ec2_security_group_ingress.IpProtocol.tcp` |
| `udp` | `aws.ec2_security_group_ingress.IpProtocol.udp` |
| `icmp` | `aws.ec2_security_group_ingress.IpProtocol.icmp` |
| `icmpv6` | `aws.ec2_security_group_ingress.IpProtocol.icmpv6` |
| `-1` | `aws.ec2_security_group_ingress.IpProtocol.all` |

Shorthand formats: `tcp` or `IpProtocol.tcp`

## Attribute Reference

### `security_group_rule_id`

- **Type:** String

