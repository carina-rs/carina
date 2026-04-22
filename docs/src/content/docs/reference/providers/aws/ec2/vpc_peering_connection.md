---
title: "aws.ec2.VpcPeeringConnection"
description: "AWS EC2 vpc_peering_connection resource reference"
---


CloudFormation Type: `AWS::EC2::VPCPeeringConnection`

Describes a VPC peering connection.

## Argument Reference

### `peer_owner_id`

- **Type:** AwsAccountId
- **Required:** No

The Amazon Web Services account ID of the owner of the accepter VPC. Default: Your Amazon Web Services account ID

### `peer_vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC with which you are creating the VPC peering connection. You must specify this parameter in the request.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the requester VPC. You must specify this parameter in the request.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Attribute Reference

### `vpc_peering_connection_id`

- **Type:** vpc_peering_connection_id

