---
title: "aws.s3.BucketAcl"
description: "AWS S3 BucketAcl resource reference"
---


CloudFormation Type: `AWS::S3::BucketAcl`

## Argument Reference

### `acl`

- **Type:** [Enum (ACL)](#acl-acl)
- **Required:** Yes

The canned ACL to apply to the bucket.

### `bucket`

- **Type:** String
- **Required:** Yes

The bucket to which to apply the ACL.

## Enum Values

### acl (ACL)

| Value | DSL Identifier |
|-------|----------------|
| `authenticated-read` | `aws.s3.BucketAcl.ACL.authenticated_read` |
| `private` | `aws.s3.BucketAcl.ACL.private` |
| `public-read` | `aws.s3.BucketAcl.ACL.public_read` |
| `public-read-write` | `aws.s3.BucketAcl.ACL.public_read_write` |

Shorthand formats: `authenticated_read` or `ACL.authenticated_read`

