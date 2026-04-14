---
title: "aws.sts.caller_identity"
description: "AWS STS caller_identity resource reference"
---


CloudFormation Type: `AWS::STS::CallerIdentity`

This is a **data source** (read-only). Use with the `read` keyword.

## Attributes

### `account_id`

- **Type:** String
- **Read-only**

<p>The Amazon Web Services account ID number of the account that owns or contains the calling
         entity.</p>

### `arn`

- **Type:** String
- **Read-only**

<p>The Amazon Web Services ARN associated with the calling entity.</p>

### `user_id`

- **Type:** String
- **Read-only**

<p>The unique identifier of the calling entity. The exact value depends on the type of
         entity that is making the call. The values returned are those listed in the <b>aws:userid</b> column in the <a href="https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_variables.html#principaltable">Principal
            table</a> found on the <b>Policy Variables</b> reference
         page in the <i>IAM User Guide</i>.</p>

