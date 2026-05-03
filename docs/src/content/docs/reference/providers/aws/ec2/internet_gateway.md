---
title: "aws.ec2.InternetGateway"
description: "AWS EC2 InternetGateway resource reference"
---


CloudFormation Type: `AWS::EC2::InternetGateway`

Describes an internet gateway.

## Example

```crn
let vpc = aws.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Environment = 'example'
  }
}

aws.ec2.InternetGateway {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = 'example'
  }
}
```

## Argument Reference

### `vpc_id`

- **Type:** VpcId
- **Required:** No

The ID of the VPC to attach the internet gateway to. The provider attaches the IGW after creation and detaches before deletion.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Attribute Reference

### `internet_gateway_id`

- **Type:** InternetGatewayId

