---
title: "awscc.ec2.InternetGateway"
description: "AWSCC EC2 InternetGateway resource reference"
---


CloudFormation Type: `AWS::EC2::InternetGateway`

Allocates an internet gateway for use with a VPC. After creating the Internet gateway, you then attach it to a VPC.

## Example

```crn
awscc.ec2.InternetGateway {
  tags = {
    Environment = 'example'
  }
}
```

## Argument Reference

### `tags`

- **Type:** Map(String)
- **Required:** No

Any tags to assign to the internet gateway.

## Attribute Reference

### `internet_gateway_id`

- **Type:** InternetGatewayId



