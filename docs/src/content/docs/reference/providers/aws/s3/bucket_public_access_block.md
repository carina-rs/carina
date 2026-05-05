---
title: "aws.s3.BucketPublicAccessBlock"
description: "AWS S3 BucketPublicAccessBlock resource reference"
---


CloudFormation Type: `AWS::S3::BucketPublicAccessBlock`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the Amazon S3 bucket whose PublicAccessBlock configuration you want to set.

### `block_public_acls`

- **Type:** Bool
- **Required:** No

Block public ACLs on the bucket and its objects.

### `ignore_public_acls`

- **Type:** Bool
- **Required:** No

Ignore any public ACLs on the bucket and its objects.

### `block_public_policy`

- **Type:** Bool
- **Required:** No

Block public bucket policies for the bucket.

### `restrict_public_buckets`

- **Type:** Bool
- **Required:** No

Restrict access to the bucket to AWS service principals and authorized users when a public policy is set.

