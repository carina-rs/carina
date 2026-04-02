---
title: "awscc.ec2.vpc_gateway_attachment"
description: "AWSCC EC2 vpc_gateway_attachment resource reference"
---


CloudFormation Type: `AWS::EC2::VPCGatewayAttachment`

Resource Type definition for AWS::EC2::VPCGatewayAttachment

## Argument Reference

### `internet_gateway_id`

- **Type:** InternetGatewayId
- **Required:** No

The ID of the internet gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes
- **Create-only:** Yes

The ID of the VPC.

### `vpn_gateway_id`

- **Type:** VpnGatewayId
- **Required:** No

The ID of the virtual private gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.

## Attribute Reference

### `attachment_type`

- **Type:** String

Used to identify if this resource is an Internet Gateway or Vpn Gateway Attachment 

