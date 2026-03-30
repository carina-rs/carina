---
title: "aws.ec2.route_table"
description: "AWS EC2 route_table resource reference"
---

# aws.ec2.route_table

CloudFormation Type: `AWS::EC2::RouteTable`

Describes a route table.

## Example

```crn
let vpc = aws.ec2.vpc {
  cidr_block = "10.0.0.0/16"

  tags = {
    Environment = "example"
  }
}

let rt = aws.ec2.route_table {
  vpc_id = vpc.vpc_id

  tags = {
    Environment = "example"
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

- **Type:** route_table_id

