---
title: "awscc.ec2.egress_only_internet_gateway"
description: "AWSCC EC2 egress_only_internet_gateway resource reference"
---


CloudFormation Type: `AWS::EC2::EgressOnlyInternetGateway`

Resource Type definition for AWS::EC2::EgressOnlyInternetGateway

## Argument Reference

### `tags`

- **Type:** Map(String)
- **Required:** No

Any tags assigned to the egress only internet gateway.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes
- **Create-only:** Yes

The ID of the VPC for which to create the egress-only internet gateway.

## Attribute Reference

### `id`

- **Type:** EgressOnlyInternetGatewayId

Service Generated ID of the EgressOnlyInternetGateway

