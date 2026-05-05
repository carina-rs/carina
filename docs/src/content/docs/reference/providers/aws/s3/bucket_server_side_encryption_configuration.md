---
title: "aws.s3.BucketServerSideEncryptionConfiguration"
description: "AWS S3 BucketServerSideEncryptionConfiguration resource reference"
---


CloudFormation Type: `AWS::S3::BucketServerSideEncryptionConfiguration`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

Specifies default encryption for a bucket using server-side encryption with different key options. Directory buckets - When you use this operation with a directory bucket, you must use path-style requests in the format https://s3express-control.region-code.amazonaws.com/bucket-name . Virtual-hosted-style requests aren't supported. Directory bucket names must be unique in the chosen Zone (Availability Zone or Local Zone). Bucket names must also follow the format bucket-base-name--zone-id--x-s3 (for example, DOC-EXAMPLE-BUCKET--usw2-az1--x-s3). For information about bucket naming restrictions, see Directory bucket naming rules in the Amazon S3 User Guide

### `rules`

- **Type:** BucketEncryptionRules
- **Required:** No

List of server-side encryption rules to apply to the bucket.

