---
title: "aws.s3.BucketReplicationConfiguration"
description: "AWS S3 BucketReplicationConfiguration resource reference"
---


CloudFormation Type: `AWS::S3::BucketReplicationConfiguration`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket

### `role`

- **Type:** String
- **Required:** No

ARN of the IAM role S3 uses to perform the replication.

### `rules`

- **Type:** BucketReplicationRules
- **Required:** No

Replication rules — at least one is required.

