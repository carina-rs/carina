---
title: "awscc.organizations.account"
description: "AWSCC ORGANIZATIONS account resource reference"
---


CloudFormation Type: `AWS::Organizations::Account`

You can use AWS::Organizations::Account to manage accounts in organization.

## Argument Reference

### `account_name`

- **Type:** String(pattern, len: 1..=50)
- **Required:** Yes

The friendly name of the member account.

### `email`

- **Type:** String(pattern, len: 6..=64)
- **Required:** Yes

The email address of the owner to assign to the new member account.

### `parent_ids`

- **Type:** `List<String>`
- **Required:** No

List of parent nodes for the member account. Currently only one parent at a time is supported. Default is root.

### `role_name`

- **Type:** String(pattern, len: 1..=64)
- **Required:** No
- **Write-only:** Yes
- **Default:** `"OrganizationAccountAccessRole"`

The name of an IAM role that AWS Organizations automatically preconfigures in the new member account. Default name is OrganizationAccountAccessRole if not specified.

### `tags`

- **Type:** Map(String)
- **Required:** No

A list of tags that you want to attach to the newly created account. For each tag in the list, you must specify both a tag key and a value.

## Enum Values

### joined_method (JoinedMethod)

| Value | DSL Identifier |
|-------|----------------|
| `INVITED` | `awscc.organizations.account.JoinedMethod.INVITED` |
| `CREATED` | `awscc.organizations.account.JoinedMethod.CREATED` |

Shorthand formats: `INVITED` or `JoinedMethod.INVITED`

### state (State)

| Value | DSL Identifier |
|-------|----------------|
| `PENDING_ACTIVATION` | `awscc.organizations.account.State.PENDING_ACTIVATION` |
| `ACTIVE` | `awscc.organizations.account.State.ACTIVE` |
| `SUSPENDED` | `awscc.organizations.account.State.SUSPENDED` |
| `PENDING_CLOSURE` | `awscc.organizations.account.State.PENDING_CLOSURE` |
| `CLOSED` | `awscc.organizations.account.State.CLOSED` |

Shorthand formats: `PENDING_ACTIVATION` or `State.PENDING_ACTIVATION`

### status (Status)

| Value | DSL Identifier |
|-------|----------------|
| `ACTIVE` | `awscc.organizations.account.Status.ACTIVE` |
| `SUSPENDED` | `awscc.organizations.account.Status.SUSPENDED` |
| `PENDING_CLOSURE` | `awscc.organizations.account.Status.PENDING_CLOSURE` |

Shorthand formats: `ACTIVE` or `Status.ACTIVE`

## Attribute Reference

### `account_id`

- **Type:** AwsAccountId

If the account was created successfully, the unique identifier (ID) of the new account.

### `arn`

- **Type:** Arn

The Amazon Resource Name (ARN) of the account.

### `joined_method`

- **Type:** [Enum (JoinedMethod)](#joined_method-joinedmethod)

The method by which the account joined the organization.

### `joined_timestamp`

- **Type:** String

The date the account became a part of the organization.

### `state`

- **Type:** [Enum (State)](#state-state)

The state of the account in the organization.

### `status`

- **Type:** [Enum (Status)](#status-status)

The status of the account in the organization.

