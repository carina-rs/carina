---
title: "aws.ec2.RouteTable"
description: "AWS EC2 RouteTable resource reference"
---


CloudFormation Type: `AWS::EC2::RouteTable`

Describes a route table.

## Example

```crn
let vpc = aws.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Environment = 'example'
  }
}

aws.ec2.RouteTable {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = 'example'
  }
}
```

## Argument Reference

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Attribute Reference

### `route_table_id`

- **Type:** RouteTableId

