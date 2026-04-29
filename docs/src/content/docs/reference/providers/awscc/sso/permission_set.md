---
title: "awscc.sso.PermissionSet"
description: "AWSCC SSO PermissionSet resource reference"
---


CloudFormation Type: `AWS::SSO::PermissionSet`

Resource Type definition for SSO PermissionSet

## Argument Reference

### `customer_managed_policy_references`

- **Type:** [List\<CustomerManagedPolicyReference\>](#customermanagedpolicyreference) (items: ..=20)
- **Required:** No

### `description`

- **Type:** String(pattern, len: 1..=700)
- **Required:** No

The permission set description.

### `inline_policy`

- **Type:** Map(String)
- **Required:** No

The inline policy to put in permission set.

### `instance_arn`

- **Type:** String(pattern, len: 10..=1224)
- **Required:** Yes
- **Create-only:** Yes

The sso instance arn that the permission set is owned.

### `managed_policies`

- **Type:** `List<String>` (items: ..=20)
- **Required:** No

### `name`

- **Type:** String(pattern, len: 1..=32)
- **Required:** Yes
- **Create-only:** Yes

The name you want to assign to this permission set.

### `permissions_boundary`

- **Type:** [Struct(PermissionsBoundary)](#permissionsboundary)
- **Required:** No

### `relay_state_type`

- **Type:** String(pattern, len: 1..=240)
- **Required:** No

The relay state URL that redirect links to any service in the AWS Management Console.

### `session_duration`

- **Type:** String(pattern, len: 1..=100)
- **Required:** No

The length of time that a user can be signed in to an AWS account.

### `tags`

- **Type:** Map(String)
- **Required:** No

## Struct Definitions

### CustomerManagedPolicyReference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | String(pattern, len: 1..=128) | Yes |  |
| `path` | String(pattern, len: 1..=512) | No |  |

### PermissionsBoundary

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `customer_managed_policy_reference` | [Struct(CustomerManagedPolicyReference)](#customermanagedpolicyreference) | No |  |
| `managed_policy_arn` | Arn | No |  |

## Attribute Reference

### `permission_set_arn`

- **Type:** String(pattern, len: 10..=1224)

The permission set that the policy will be attached to

