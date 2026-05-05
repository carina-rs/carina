---
title: "aws.s3.Bucket"
description: "AWS S3 Bucket resource reference"
---


CloudFormation Type: `AWS::S3::Bucket`

## Example

```crn
let bucket = aws.s3.Bucket {
  bucket = 'carina-example-s3-bucket'

  tags = {
    Environment = 'example'
  }
}

aws.s3.BucketVersioning {
  bucket = bucket.bucket
  status = aws.s3.BucketVersioning.VersioningStatus.Enabled
}
```

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket to create. General purpose buckets - For information about bucket naming restrictions, see Bucket naming rules in the Amazon S3 User Guide. Directory buckets - When you use this operation with a directory bucket, you must use path-style requests in the format https://s3express-control.region-code.amazonaws.com/bucket-name . Virtual-hosted-style requests aren't supported. Directory bucket names must be unique in the chosen Zone (Availability Zone or Local Zone). Bucket names must also follow the format bucket-base-name--zone-id--x-s3 (for example, DOC-EXAMPLE-BUCKET--usw2-az1--x-s3). For information about bucket naming restrictions, see Directory bucket naming rules in the Amazon S3 User Guide

### `bucket_namespace`

- **Type:** [Enum (BucketNamespace)](#bucket_namespace-bucketnamespace)
- **Required:** No

Specifies the namespace where you want to create your general purpose bucket. When you create a general purpose bucket, you can choose to create a bucket in the shared global namespace or you can choose to create a bucket in your account regional namespace. Your account regional namespace is a subdivision of the global namespace that only your account can create buckets in. For more information on bucket namespaces, see Namespaces for general purpose buckets. General purpose buckets in your account regional namespace must follow a specific naming convention. These buckets consist of a bucket name prefix that you create, and a suffix that contains your 12-digit Amazon Web Services Account ID, the Amazon Web Services Region code, and ends with -an. Bucket names must follow the format bucket-name-prefix-accountId-region-an (for example, amzn-s3-demo-bucket-111122223333-us-west-2-an). For information about bucket naming restrictions, see Account regional namespace naming rules in the Amazon S3 User Guide. This functionality is not supported for directory buckets.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### bucket_namespace (BucketNamespace)

| Value | DSL Identifier |
|-------|----------------|
| `account-regional` | `aws.s3.Bucket.BucketNamespace.account_regional` |
| `global` | `aws.s3.Bucket.BucketNamespace.global` |

Shorthand formats: `account_regional` or `BucketNamespace.account_regional`

