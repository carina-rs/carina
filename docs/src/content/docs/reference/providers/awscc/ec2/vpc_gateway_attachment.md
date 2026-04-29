---
title: "awscc.ec2.VpcGatewayAttachment"
description: "AWSCC EC2 VpcGatewayAttachment resource reference"
---


CloudFormation Type: `AWS::EC2::VPCGatewayAttachment`

Resource Type definition for AWS::EC2::VPCGatewayAttachment

## Example

```crn
let vpc = awscc.ec2.Vpc {
  cidr_block           = '10.0.0.0/16'
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let igw = awscc.ec2.InternetGateway {}

awscc.ec2.VpcGatewayAttachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}
```

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

