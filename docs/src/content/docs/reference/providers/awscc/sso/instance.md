---
title: "awscc.sso.Instance"
description: "AWSCC SSO Instance resource reference"
---


CloudFormation Type: `AWS::SSO::Instance`

Resource Type definition for Identity Center (SSO) Instance

## Argument Reference

### `name`

- **Type:** String(pattern, len: 1..=32)
- **Required:** No

The name you want to assign to this Identity Center (SSO) Instance

### `tags`

- **Type:** Map(String)
- **Required:** No

## Enum Values

### status (Status)

| Value | DSL Identifier |
|-------|----------------|
| `CREATE_IN_PROGRESS` | `awscc.sso.Instance.Status.CREATE_IN_PROGRESS` |
| `DELETE_IN_PROGRESS` | `awscc.sso.Instance.Status.DELETE_IN_PROGRESS` |
| `ACTIVE` | `awscc.sso.Instance.Status.ACTIVE` |

Shorthand formats: `CREATE_IN_PROGRESS` or `Status.CREATE_IN_PROGRESS`

## Attribute Reference

### `identity_store_id`

- **Type:** String(pattern, len: 1..=64)

The ID of the identity store associated with the created Identity Center (SSO) Instance

### `instance_arn`

- **Type:** String(pattern, len: 10..=1224)

The SSO Instance ARN that is returned upon creation of the Identity Center (SSO) Instance

### `owner_account_id`

- **Type:** AwsAccountId

The AWS accountId of the owner of the Identity Center (SSO) Instance

### `status`

- **Type:** [Enum (Status)](#status-status)

The status of the Identity Center (SSO) Instance, create_in_progress/delete_in_progress/active

