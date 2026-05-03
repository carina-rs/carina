---
title: "aws.ec2.TransitGatewayAttachment"
description: "AWS EC2 TransitGatewayAttachment resource reference"
---


CloudFormation Type: `AWS::EC2::TransitGatewayAttachment`

Describes a VPC attachment.

## Argument Reference

### `options`

- **Type:** [Struct(CreateTransitGatewayVpcAttachmentRequestOptions)](#createtransitgatewayvpcattachmentrequestoptions)
- **Required:** No

The VPC attachment options.

### `subnet_ids`

- **Type:** `List<SubnetId>`
- **Required:** Yes

The IDs of one or more subnets. You can specify only one subnet per Availability Zone. You must specify at least one subnet, but we recommend that you specify two subnets for better availability. The transit gateway uses one IP address from each specified subnet.

### `transit_gateway_id`

- **Type:** TransitGatewayId
- **Required:** Yes

The ID of the transit gateway.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Struct Definitions

### CreateTransitGatewayVpcAttachmentRequestOptions

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `appliance_mode_support` | [Enum (ApplianceModeSupport)](#appliance_mode_support-appliancemodesupport) | No | Enable or disable support for appliance mode. If enabled, a traffic flow between a source and destination uses the same Availability Zone for the VPC attachment for the lifetime of that flow. The default is disable. |
| `dns_support` | [Enum (DnsSupport)](#dns_support-dnssupport) | No | Enable or disable DNS support. The default is enable. |
| `ipv6_support` | [Enum (Ipv6Support)](#ipv6_support-ipv6support) | No | Enable or disable IPv6 support. The default is disable. |
| `security_group_referencing_support` | [Enum (SecurityGroupReferencingSupport)](#security_group_referencing_support-securitygroupreferencingsupport) | No | Enables you to reference a security group across VPCs attached to a transit gateway to simplify security group management. This option is set to enable by default. However, at the transit gateway level the default is set to disable. For more information about security group referencing, see Security group referencing in the Amazon Web Services Transit Gateways Guide. |

## Attribute Reference

### `transit_gateway_attachment_id`

- **Type:** String

