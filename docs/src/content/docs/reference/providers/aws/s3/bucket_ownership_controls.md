---
title: "aws.s3.BucketOwnershipControls"
description: "AWS S3 BucketOwnershipControls resource reference"
---


CloudFormation Type: `AWS::S3::BucketOwnershipControls`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the Amazon S3 bucket whose OwnershipControls you want to set.

### `object_ownership`

- **Type:** AttributeType::StringEnum { name: "ObjectOwnership".toString(), values: vec!["BucketOwnerEnforced".toString(), "BucketOwnerPreferred".toString(), "ObjectWriter".toString()], namespace: Some("aws.s3.BucketOwnershipControls".toString()), toDsl: None }
- **Required:** No

Object ownership setting: BucketOwnerEnforced, BucketOwnerPreferred, or ObjectWriter.

