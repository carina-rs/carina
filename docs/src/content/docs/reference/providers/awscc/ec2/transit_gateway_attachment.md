---
title: "awscc.ec2.transit_gateway_attachment"
description: "AWSCC EC2 transit_gateway_attachment resource reference"
---


CloudFormation Type: `AWS::EC2::TransitGatewayAttachment`

Resource Type definition for AWS::EC2::TransitGatewayAttachment

## Example

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}

let subnet = awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = "ap-northeast-1a"
}

let tgw = awscc.ec2.transit_gateway {
  description = "Example Transit Gateway"
}

awscc.ec2.transit_gateway_attachment {
  transit_gateway_id = tgw.id
  vpc_id             = vpc.vpc_id
  subnet_ids         = [subnet.subnet_id]

  tags = {
    Environment = "example"
  }
}
```

## Argument Reference

### `options`

- **Type:** [Struct(Options)](#options)
- **Required:** No

The options for the transit gateway vpc attachment.

### `subnet_ids`

- **Type:** `List<SubnetId>`
- **Required:** Yes

### `tags`

- **Type:** Map(String)
- **Required:** No

### `transit_gateway_id`

- **Type:** TransitGatewayId
- **Required:** Yes
- **Create-only:** Yes

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes
- **Create-only:** Yes

## Enum Values

### appliance_mode_support (ApplianceModeSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway_attachment.ApplianceModeSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway_attachment.ApplianceModeSupport.disable` |

Shorthand formats: `enable` or `ApplianceModeSupport.enable`

### dns_support (DnsSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway_attachment.DnsSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway_attachment.DnsSupport.disable` |

Shorthand formats: `enable` or `DnsSupport.enable`

### ipv6_support (Ipv6Support)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway_attachment.Ipv6Support.enable` |
| `disable` | `awscc.ec2.transit_gateway_attachment.Ipv6Support.disable` |

Shorthand formats: `enable` or `Ipv6Support.enable`

### security_group_referencing_support (SecurityGroupReferencingSupport)

| Value | DSL Identifier |
|-------|----------------|
| `enable` | `awscc.ec2.transit_gateway_attachment.SecurityGroupReferencingSupport.enable` |
| `disable` | `awscc.ec2.transit_gateway_attachment.SecurityGroupReferencingSupport.disable` |

Shorthand formats: `enable` or `SecurityGroupReferencingSupport.enable`

## Struct Definitions

### Options

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `appliance_mode_support` | [Enum (ApplianceModeSupport)](#appliance_mode_support-appliancemodesupport) | No | Indicates whether to enable Ipv6 Support for Vpc Attachment. Valid Values: enable | disable |
| `dns_support` | [Enum (DnsSupport)](#dns_support-dnssupport) | No | Indicates whether to enable DNS Support for Vpc Attachment. Valid Values: enable | disable |
| `ipv6_support` | [Enum (Ipv6Support)](#ipv6_support-ipv6support) | No | Indicates whether to enable Ipv6 Support for Vpc Attachment. Valid Values: enable | disable |
| `security_group_referencing_support` | [Enum (SecurityGroupReferencingSupport)](#security_group_referencing_support-securitygroupreferencingsupport) | No | Indicates whether to enable Security Group referencing support for Vpc Attachment. Valid Values: enable | disable |

## Attribute Reference

### `id`

- **Type:** TransitGatewayAttachmentId

