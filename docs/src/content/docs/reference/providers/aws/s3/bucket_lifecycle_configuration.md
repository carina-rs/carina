---
title: "aws.s3.BucketLifecycleConfiguration"
description: "AWS S3 BucketLifecycleConfiguration resource reference"
---


CloudFormation Type: `AWS::S3::BucketLifecycleConfiguration`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket for which to set the configuration.

### `rules`

- **Type:** BucketLifecycleRules
- **Required:** No

Lifecycle rules to apply to the bucket.

