---
title: "aws.s3.BucketPolicy"
description: "AWS S3 BucketPolicy resource reference"
---


CloudFormation Type: `AWS::S3::BucketPolicy`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket. Directory buckets - When you use this operation with a directory bucket, you must use path-style requests in the format https://s3express-control.region-code.amazonaws.com/bucket-name . Virtual-hosted-style requests aren't supported. Directory bucket names must be unique in the chosen Zone (Availability Zone or Local Zone). Bucket names must also follow the format bucket-base-name--zone-id--x-s3 (for example, DOC-EXAMPLE-BUCKET--usw2-az1--x-s3). For information about bucket naming restrictions, see Directory bucket naming rules in the Amazon S3 User Guide

### `policy`

- **Type:** IamPolicyDocument
- **Required:** Yes

The bucket policy as a JSON document. For directory buckets, the only IAM action supported in the bucket policy is s3express:CreateSession.

