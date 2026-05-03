---
title: "aws.ec2.EgressOnlyInternetGateway"
description: "AWS EC2 EgressOnlyInternetGateway resource reference"
---


CloudFormation Type: `AWS::EC2::EgressOnlyInternetGateway`

Describes an egress-only internet gateway.

## Argument Reference

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC for which to create the egress-only internet gateway.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Attribute Reference

### `egress_only_internet_gateway_id`

- **Type:** InternetGatewayId

