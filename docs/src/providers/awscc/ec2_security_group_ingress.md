# awscc.ec2_security_group_ingress

CloudFormation Type: `AWS::EC2::SecurityGroupIngress`

Resource Type definition for AWS::EC2::SecurityGroupIngress

## Argument Reference

### `cidr_ip`

- **Type:** Ipv4Cidr
- **Required:** No

The IPv4 ranges

### `cidr_ipv6`

- **Type:** Ipv6Cidr
- **Required:** No

[VPC only] The IPv6 ranges

### `description`

- **Type:** String
- **Required:** No

Updates the description of an ingress (inbound) security group rule. You can replace an existing description, or add a description to a rule that did not have one previously

### `from_port`

- **Type:** Int
- **Required:** No

The start of port range for the TCP and UDP protocols, or an ICMP/ICMPv6 type number. A value of -1 indicates all ICMP/ICMPv6 types. If you specify all ICMP/ICMPv6 types, you must specify all codes. Use this for ICMP and any protocol that uses ports.

### `group_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the security group. You must specify either the security group ID or the security group name in the request. For security groups in a nondefault VPC, you must specify the security group ID. You must specify the GroupName property or the GroupId property. For security groups that are in a VPC, you must use the GroupId property.

### `group_name`

- **Type:** String
- **Required:** No

The name of the security group.

### `ip_protocol`

- **Type:** [Enum (IpProtocol)](#ip_protocol-ipprotocol)
- **Required:** Yes

The IP protocol name (tcp, udp, icmp, icmpv6) or number (see Protocol Numbers). [VPC only] Use -1 to specify all protocols. When authorizing security group rules, specifying -1 or a protocol number other than tcp, udp, icmp, or icmpv6 allows traffic on all ports, regardless of any port range you specify. For tcp, udp, and icmp, you must specify a port range. For icmpv6, the port range is optional; if you omit the port range, traffic for all types and codes is allowed.

### `source_prefix_list_id`

- **Type:** AwsResourceId
- **Required:** No

[EC2-VPC only] The ID of a prefix list. 

### `source_security_group_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the security group. You must specify either the security group ID or the security group name. For security groups in a nondefault VPC, you must specify the security group ID.

### `source_security_group_name`

- **Type:** String
- **Required:** No

[EC2-Classic, default VPC] The name of the source security group. You must specify the GroupName property or the GroupId property. For security groups that are in a VPC, you must use the GroupId property.

### `source_security_group_owner_id`

- **Type:** String
- **Required:** No

[nondefault VPC] The AWS account ID that owns the source security group. You can't specify this property with an IP address range. If you specify SourceSecurityGroupName or SourceSecurityGroupId and that security group is owned by a different account than the account creating the stack, you must specify the SourceSecurityGroupOwnerId; otherwise, this property is optional.

### `to_port`

- **Type:** Int
- **Required:** No

The end of port range for the TCP and UDP protocols, or an ICMP/ICMPv6 code. A value of -1 indicates all ICMP/ICMPv6 codes for the specified ICMP type. If you specify all ICMP/ICMPv6 types, you must specify all codes. Use this for ICMP and any protocol that uses ports.

## Enum Values

### ip_protocol (IpProtocol)

| Value | DSL Identifier |
|-------|----------------|
| `tcp` | `awscc.ec2_security_group_ingress.IpProtocol.tcp` |
| `udp` | `awscc.ec2_security_group_ingress.IpProtocol.udp` |
| `icmp` | `awscc.ec2_security_group_ingress.IpProtocol.icmp` |
| `icmpv6` | `awscc.ec2_security_group_ingress.IpProtocol.icmpv6` |
| `-1` | `awscc.ec2_security_group_ingress.IpProtocol.-1` |

Shorthand formats: `tcp` or `IpProtocol.tcp`

## Attribute Reference

### `id`

- **Type:** String



## Example

```crn
let vpc = awscc.ec2_vpc {
  name       = "example-vpc"
  cidr_block = "10.0.0.0/16"
}

let sg = awscc.ec2_security_group {
  name              = "example-sg"
  vpc_id            = vpc.vpc_id
  group_description = "Example security group"
}

awscc.ec2_security_group_ingress {
  name        = "example-https-ingress"
  group_id    = sg.group_id
  description = "Allow HTTPS from VPC"
  ip_protocol = "tcp"
  from_port   = 443
  to_port     = 443
  cidr_ip     = "10.0.0.0/16"
}
```
