# awscc.ec2_vpc_peering_connection

CloudFormation Type: `AWS::EC2::VPCPeeringConnection`

Resource Type definition for AWS::EC2::VPCPeeringConnection

## Argument Reference

### `peer_owner_id`

- **Type:** String
- **Required:** No

The AWS account ID of the owner of the accepter VPC.

### `peer_region`

- **Type:** String
- **Required:** No

The Region code for the accepter VPC, if the accepter VPC is located in a Region other than the Region in which you make the request.

### `peer_role_arn`

- **Type:** IamRoleArn
- **Required:** No

The Amazon Resource Name (ARN) of the VPC peer role for the peering connection in another AWS account.

### `peer_vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC with which you are creating the VPC peering connection. You must specify this parameter in the request.

### `tags`

- **Type:** Map
- **Required:** No

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC.

## Attribute Reference

### `id`

- **Type:** String



## Example

```crn
let vpc1 = awscc.ec2_vpc {
  cidr_block = "10.0.0.0/16"
}

let vpc2 = awscc.ec2_vpc {
  cidr_block = "10.1.0.0/16"
}

let peering = awscc.ec2_vpc_peering_connection {
  vpc_id      = vpc1.vpc_id
  peer_vpc_id = vpc2.vpc_id

  tags = {
    Environment = "example"
  }
}
```
