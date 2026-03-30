---
title: "aws.ec2.internet_gateway"
description: "AWS EC2 internet_gateway resource reference"
---

# aws.ec2.internet_gateway

CloudFormation Type: `AWS::EC2::InternetGateway`

Describes an internet gateway.

## Example

```crn
let vpc = aws.ec2.vpc {
  cidr_block = "10.0.0.0/16"

  tags = {
    Environment = "example"
  }
}

let igw = aws.ec2.internet_gateway {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
  }
}
```

## Argument Reference

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Attribute Reference

### `internet_gateway_id`

- **Type:** internet_gateway_id

