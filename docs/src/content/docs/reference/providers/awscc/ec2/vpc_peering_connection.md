---
title: "awscc.ec2.vpc_peering_connection"
description: "AWSCC EC2 vpc_peering_connection resource reference"
---


CloudFormation Type: `AWS::EC2::VPCPeeringConnection`

Resource Type definition for AWS::EC2::VPCPeeringConnection

## Argument Reference

### `assume_role_region`

- **Type:** Region
- **Required:** No
- **Create-only:** Yes
- **Write-only:** Yes

The Region code to use when calling Security Token Service (STS) to assume the PeerRoleArn, if provided.

### `peer_owner_id`

- **Type:** AwsAccountId
- **Required:** No
- **Create-only:** Yes

The AWS account ID of the owner of the accepter VPC.

### `peer_region`

- **Type:** Region
- **Required:** No
- **Create-only:** Yes

The Region code for the accepter VPC, if the accepter VPC is located in a Region other than the Region in which you make the request.

### `peer_role_arn`

- **Type:** IamRoleArn
- **Required:** No
- **Create-only:** Yes
- **Write-only:** Yes

The Amazon Resource Name (ARN) of the VPC peer role for the peering connection in another AWS account.

### `peer_vpc_id`

- **Type:** VpcId
- **Required:** Yes
- **Create-only:** Yes

The ID of the VPC with which you are creating the VPC peering connection. You must specify this parameter in the request.

### `tags`

- **Type:** Map(String)
- **Required:** No

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes
- **Create-only:** Yes

The ID of the VPC.

## Attribute Reference

### `id`

- **Type:** VpcPeeringConnectionId

