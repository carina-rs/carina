# aws.ec2.security_group

CloudFormation Type: `AWS::EC2::SecurityGroup`

Describes a security group.

## Argument Reference

### `description`

- **Type:** String
- **Required:** Yes

A description for the security group.     Constraints: Up to 255 characters in length     Valid characters: a-z, A-Z, 0-9, spaces, and ._-:/()#,@[]+=&;{}!$*

### `group_name`

- **Type:** String
- **Required:** Yes

The name of the security group. Names are case-insensitive and must be unique within the VPC.     Constraints: Up to 255 characters in length. Can't start with sg-.     Valid characters: a-z, A-Z, 0-9, spaces, and ._-:/()#,@[]+=&;{}!$*

### `vpc_id`

- **Type:** VpcId
- **Required:** No

The ID of the VPC. Required for a nondefault VPC.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Attribute Reference

### `group_id`

- **Type:** SecurityGroupId

