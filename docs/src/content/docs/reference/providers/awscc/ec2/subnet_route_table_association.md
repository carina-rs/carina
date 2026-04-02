---
title: "awscc.ec2.subnet_route_table_association"
description: "AWSCC EC2 subnet_route_table_association resource reference"
---


CloudFormation Type: `AWS::EC2::SubnetRouteTableAssociation`

Associates a subnet with a route table. The subnet and route table must be in the same VPC. This association causes traffic originating from the subnet to be routed according to the routes in the route table. A route table can be associated with multiple subnets. To create a route table, see [AWS::EC2::RouteTable](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-routetable.html).

## Argument Reference

### `route_table_id`

- **Type:** RouteTableId
- **Required:** Yes
- **Create-only:** Yes

The ID of the route table. The physical ID changes when the route table ID is changed.

### `subnet_id`

- **Type:** SubnetId
- **Required:** Yes
- **Create-only:** Yes

The ID of the subnet.

## Attribute Reference

### `id`

- **Type:** SubnetRouteTableAssociationId



