---
title: "aws.s3.BucketVersioning"
description: "AWS S3 BucketVersioning resource reference"
---


CloudFormation Type: `AWS::S3::BucketVersioning`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The bucket name.

### `status`

- **Type:** AttributeType::StringEnum { name: "VersioningStatus".toString(), values: vec!["Enabled".toString(), "Suspended".toString()], namespace: Some("aws.s3.BucketVersioning".toString()), dslAliases: vec![("Enabled".toString(), "enabled".toString()), ("Suspended".toString(), "suspended".toString())] }
- **Required:** No

Versioning state of the bucket: Enabled or Suspended.

### `mfa_delete`

- **Type:** AttributeType::StringEnum { name: "MFADelete".toString(), values: vec!["Enabled".toString(), "Disabled".toString()], namespace: Some("aws.s3.BucketVersioning".toString()), dslAliases: vec![("Enabled".toString(), "enabled".toString()), ("Disabled".toString(), "disabled".toString())] }
- **Required:** No

MFA-delete state. Specifies whether MFA delete is enabled in the bucket versioning configuration.

