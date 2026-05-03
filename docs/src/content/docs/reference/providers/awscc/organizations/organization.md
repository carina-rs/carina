---
title: "awscc.organizations.Organization"
description: "AWSCC Organizations Organization resource reference"
---


CloudFormation Type: `AWS::Organizations::Organization`

Resource schema for AWS::Organizations::Organization

## Argument Reference

### `feature_set`

- **Type:** [Enum (FeatureSet)](#feature_set-featureset)
- **Required:** No
- **Default:** `"ALL"`

Specifies the feature set supported by the new organization. Each feature set supports different levels of functionality.

## Enum Values

### feature_set (FeatureSet)

| Value | DSL Identifier |
|-------|----------------|
| `ALL` | `awscc.organizations.Organization.FeatureSet.ALL` |
| `CONSOLIDATED_BILLING` | `awscc.organizations.Organization.FeatureSet.CONSOLIDATED_BILLING` |

Shorthand formats: `ALL` or `FeatureSet.ALL`

## Attribute Reference

### `arn`

- **Type:** Arn

The Amazon Resource Name (ARN) of an organization.

### `id`

- **Type:** String

The unique identifier (ID) of an organization.

### `management_account_arn`

- **Type:** Arn

The Amazon Resource Name (ARN) of the account that is designated as the management account for the organization.

### `management_account_email`

- **Type:** String(pattern, len: 6..=64)

The email address that is associated with the AWS account that is designated as the management account for the organization.

### `management_account_id`

- **Type:** AwsAccountId

The unique identifier (ID) of the management account of an organization.

### `root_id`

- **Type:** String(pattern, len: ..=64)

The unique identifier (ID) for the root.

