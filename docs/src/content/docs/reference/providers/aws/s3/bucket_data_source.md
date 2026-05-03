---
title: "aws.s3.Bucket"
description: "AWS S3 Bucket resource reference"
---


CloudFormation Type: `AWS::S3::Bucket`

This is a **data source** (read-only). Use with the `read` keyword.

## Lookup Inputs

### `bucket`

- **Required:** Yes

Name of the S3 bucket to look up.

## Attributes

### `arn`

- **Type:** Arn
- **Read-only**

ARN of the bucket.

### `region`

- **Type:** String
- **Read-only**

AWS region the bucket is in.

### `bucket_domain_name`

- **Type:** String
- **Read-only**

Bucket domain name (`<bucket>.s3.amazonaws.com`).

### `bucket_regional_domain_name`

- **Type:** String
- **Read-only**

Region-specific bucket domain name (`<bucket>.s3.<region>.amazonaws.com`).

### `hosted_zone_id`

- **Type:** String
- **Read-only**

Route 53 Hosted Zone ID for the bucket's region.

