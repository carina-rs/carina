---
title: "awscc.iam.oidc_provider"
description: "AWSCC IAM oidc_provider resource reference"
---


CloudFormation Type: `AWS::IAM::OIDCProvider`

Resource Type definition for AWS::IAM::OIDCProvider

## Argument Reference

### `client_id_list`

- **Type:** `List<String>`
- **Required:** No

### `tags`

- **Type:** Map(String)
- **Required:** No

### `thumbprint_list`

- **Type:** `List<String>` (items: ..=5)
- **Required:** No

### `url`

- **Type:** String(len: 1..=255)
- **Required:** No
- **Create-only:** Yes

## Attribute Reference

### `arn`

- **Type:** Arn

Amazon Resource Name (ARN) of the OIDC provider

