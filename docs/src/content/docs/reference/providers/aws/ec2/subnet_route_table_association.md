---
title: "aws.ec2.subnet_route_table_association"
description: "AWS EC2 subnet_route_table_association resource reference"
---


CloudFormation Type: `AWS::EC2::SubnetRouteTableAssociation`

Describes an association between a route table and a subnet or gateway.

## Argument Reference

### `public_ipv4_pool`

- **Type:** String
- **Required:** No

The ID of a public IPv4 pool. A public IPv4 pool is a pool of IPv4 addresses that you've brought to Amazon Web Services with BYOIP.

### `route_table_id`

- **Type:** route_table_id
- **Required:** Yes

The ID of the route table.

### `subnet_id`

- **Type:** SubnetId
- **Required:** Yes

The ID of the subnet.

