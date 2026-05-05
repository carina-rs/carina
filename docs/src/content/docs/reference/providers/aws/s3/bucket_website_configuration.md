---
title: "aws.s3.BucketWebsiteConfiguration"
description: "AWS S3 BucketWebsiteConfiguration resource reference"
---


CloudFormation Type: `AWS::S3::BucketWebsiteConfiguration`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The bucket name.

### `index_document`

- **Type:** S3IndexDocument
- **Required:** No

Index document for the bucket's website.

### `error_document`

- **Type:** S3ErrorDocument
- **Required:** No

Custom error document key.

### `redirect_all_requests_to`

- **Type:** S3RedirectAllRequestsTo
- **Required:** No

Redirect all bucket-website requests to another host (alternative to index_document).

