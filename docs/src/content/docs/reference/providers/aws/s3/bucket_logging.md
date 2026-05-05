---
title: "aws.s3.BucketLogging"
description: "AWS S3 BucketLogging resource reference"
---


CloudFormation Type: `AWS::S3::BucketLogging`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket for which to set the logging parameters.

### `target_bucket`

- **Type:** String
- **Required:** No

Destination bucket for server access logs.

### `target_prefix`

- **Type:** String
- **Required:** No

Key prefix to apply to log objects.

### `target_object_key_format`

- **Type:** BucketTargetObjectKeyFormat
- **Required:** No

Partitioning / simple-prefix selector for log object keys.

