---
title: "aws.organizations.account"
description: "AWS ORGANIZATIONS account resource reference"
---


CloudFormation Type: `AWS::Organizations::Account`

Contains information about an Amazon Web Services account that is a member of an organization.

## Argument Reference

### `account_name`

- **Type:** String
- **Required:** Yes

The friendly name of the member account.

### `email`

- **Type:** String
- **Required:** Yes

The email address of the owner to assign to the new member account. This email address must not already be associated with another Amazon Web Services account. You must use a valid email address to complete account creation. The rules for a valid email address: The address must be a minimum of 6 and a maximum of 64 characters long. All characters must be 7-bit ASCII characters. There must be one and only one @ symbol, which separates the local name from the domain name. The local name can't contain any of the following characters: whitespace, " ' ( ) [ ] : ; , \ | % & The local name can't begin with a dot (.) The domain name can consist of only the characters [a-z],[A-Z],[0-9], hyphen (-), or dot (.) The domain name can't begin or end with a hyphen (-) or dot (.) The domain name must contain at least one dot You can't access the root user of the account or remove an account that was created with an invalid email address.

### `iam_user_access_to_billing`

- **Type:** [Enum (IamUserAccessToBilling)](#iam_user_access_to_billing-iamuseraccesstobilling)
- **Required:** No

If set to ALLOW, the new account enables IAM users to access account billing information if they have the required permissions. If set to DENY, only the root user of the new account can access account billing information. For more information, see About IAM access to the Billing and Cost Management console in the Amazon Web Services Billing and Cost Management User Guide. If you don't specify this parameter, the value defaults to ALLOW, and IAM users and roles with the required permissions can access billing information for the new account.

### `role_name`

- **Type:** String
- **Required:** No

The name of an IAM role that Organizations automatically preconfigures in the new member account. This role trusts the management account, allowing users in the management account to assume the role, as permitted by the management account administrator. The role has administrator permissions in the new member account. If you don't specify this parameter, the role name defaults to OrganizationAccountAccessRole. For more information about how to use this role to access the member account, see the following links: Creating the OrganizationAccountAccessRole in an invited member account in the Organizations User Guide Steps 2 and 3 in IAM Tutorial: Delegate access across Amazon Web Services accounts using IAM roles in the IAM User Guide The regex pattern that is used to validate this parameter. The pattern can include uppercase letters, lowercase letters, digits with no spaces, and any of the following characters: =,.@-

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### iam_user_access_to_billing (IamUserAccessToBilling)

| Value | DSL Identifier |
|-------|----------------|
| `ALLOW` | `aws.organizations.account.IamUserAccessToBilling.ALLOW` |
| `DENY` | `aws.organizations.account.IamUserAccessToBilling.DENY` |

Shorthand formats: `ALLOW` or `IamUserAccessToBilling.ALLOW`

### joined_method (JoinedMethod)

| Value | DSL Identifier |
|-------|----------------|
| `CREATED` | `aws.organizations.account.JoinedMethod.CREATED` |
| `INVITED` | `aws.organizations.account.JoinedMethod.INVITED` |

Shorthand formats: `CREATED` or `JoinedMethod.CREATED`

### status (Status)

| Value | DSL Identifier |
|-------|----------------|
| `ACTIVE` | `aws.organizations.account.Status.ACTIVE` |
| `PENDING_CLOSURE` | `aws.organizations.account.Status.PENDING_CLOSURE` |
| `SUSPENDED` | `aws.organizations.account.Status.SUSPENDED` |

Shorthand formats: `ACTIVE` or `Status.ACTIVE`

## Attribute Reference

### `arn`

- **Type:** Arn

### `id`

- **Type:** String

### `joined_method`

- **Type:** [Enum (JoinedMethod)](#joined_method-joinedmethod)

### `joined_timestamp`

- **Type:** String

### `name`

- **Type:** String

### `status`

- **Type:** [Enum (Status)](#status-status)

