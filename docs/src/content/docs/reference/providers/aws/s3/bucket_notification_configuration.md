---
title: "aws.s3.BucketNotificationConfiguration"
description: "AWS S3 BucketNotificationConfiguration resource reference"
---


CloudFormation Type: `AWS::S3::BucketNotificationConfiguration`

## Argument Reference

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket.

### `topic_configurations`

- **Type:** BucketTopicConfigurations
- **Required:** No

SNS topic notification configurations.

### `queue_configurations`

- **Type:** BucketQueueConfigurations
- **Required:** No

SQS queue notification configurations.

### `lambda_function_configurations`

- **Type:** BucketLambdaFunctionConfigurations
- **Required:** No

Lambda function notification configurations.

### `event_bridge_configuration`

- **Type:** BucketEventBridgeConfiguration
- **Required:** No

Enables EventBridge notifications when present.

