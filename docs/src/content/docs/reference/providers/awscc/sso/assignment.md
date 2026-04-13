---
title: "awscc.sso.assignment"
description: "AWSCC SSO assignment resource reference"
---


CloudFormation Type: `AWS::SSO::Assignment`

Resource Type definition for SSO assignmet

## Argument Reference

### `instance_arn`

- **Type:** Arn
- **Required:** Yes
- **Create-only:** Yes

The sso instance that the permission set is owned.

### `permission_set_arn`

- **Type:** Arn
- **Required:** Yes
- **Create-only:** Yes

The permission set that the assignment will be assigned

### `principal_id`

- **Type:** String(pattern, len: 1..=47)
- **Required:** Yes
- **Create-only:** Yes

The assignee's identifier, user id/group id

### `principal_type`

- **Type:** [Enum (PrincipalType)](#principal_type-principaltype)
- **Required:** Yes
- **Create-only:** Yes

The assignee's type, user/group

### `target_id`

- **Type:** String
- **Required:** Yes
- **Create-only:** Yes

The account id to be provisioned.

### `target_type`

- **Type:** [Enum (TargetType)](#target_type-targettype)
- **Required:** Yes
- **Create-only:** Yes

The type of resource to be provisioned to, only aws account now

## Enum Values

### principal_type (PrincipalType)

| Value | DSL Identifier |
|-------|----------------|
| `USER` | `awscc.sso.assignment.PrincipalType.USER` |
| `GROUP` | `awscc.sso.assignment.PrincipalType.GROUP` |

Shorthand formats: `USER` or `PrincipalType.USER`

### target_type (TargetType)

| Value | DSL Identifier |
|-------|----------------|
| `AWS_ACCOUNT` | `awscc.sso.assignment.TargetType.AWS_ACCOUNT` |

Shorthand formats: `AWS_ACCOUNT` or `TargetType.AWS_ACCOUNT`

