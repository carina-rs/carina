---
title: "aws.sts.CallerIdentity"
description: "AWS STS CallerIdentity resource reference"
---


CloudFormation Type: `AWS::STS::CallerIdentity`

This is a **data source** (read-only). Use with the `read` keyword.

## Attributes

### `account_id`

- **Type:** AwsAccountId
- **Read-only**

The Amazon Web Services account ID number of the account that owns or contains the calling entity.

### `arn`

- **Type:** Arn
- **Read-only**

The Amazon Web Services ARN associated with the calling entity.

### `user_id`

- **Type:** String
- **Read-only**

The unique identifier of the calling entity.

