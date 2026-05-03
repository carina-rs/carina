---
title: "aws.organizations.Organization"
description: "AWS Organizations Organization resource reference"
---


CloudFormation Type: `AWS::Organizations::Organization`

Contains details about an organization. An organization is a collection of accounts that are centrally managed together using consolidated billing, organized hierarchically with organizational units (OUs), and controlled with policies .

## Argument Reference

### `feature_set`

- **Type:** [Enum (FeatureSet)](#feature_set-featureset)
- **Required:** No

Specifies the feature set supported by the new organization. Each feature set supports different levels of functionality. CONSOLIDATED_BILLING: All member accounts have their bills consolidated to and paid by the management account. For more information, see Consolidated billing in the Organizations User Guide. The consolidated billing feature subset isn't available for organizations in the Amazon Web Services GovCloud (US) Region. ALL: In addition to all the features supported by the consolidated billing feature set, the management account can also apply any policy type to any member account in the organization. For more information, see All features in the Organizations User Guide.

## Enum Values

### feature_set (FeatureSet)

| Value | DSL Identifier |
|-------|----------------|
| `ALL` | `aws.organizations.Organization.FeatureSet.ALL` |
| `CONSOLIDATED_BILLING` | `aws.organizations.Organization.FeatureSet.CONSOLIDATED_BILLING` |

Shorthand formats: `ALL` or `FeatureSet.ALL`

## Attribute Reference

### `arn`

- **Type:** Arn

### `id`

- **Type:** String

### `master_account_arn`

- **Type:** Arn

### `master_account_email`

- **Type:** Email

### `master_account_id`

- **Type:** AwsAccountId

