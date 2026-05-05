---
title: "aws.s3.BucketCorsConfiguration"
description: "AWS S3 BucketCorsConfiguration resource reference"
---


CloudFormation Type: `AWS::S3::BucketCorsConfiguration`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

Specifies the bucket impacted by the corsconfiguration.

### `cors_rules`

- **Type:** BucketCorsRules
- **Required:** No

CORS rules to apply to the bucket.

