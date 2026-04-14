---
title: "aws.logs.log_group"
description: "AWS LOGS log_group resource reference"
---


CloudFormation Type: `AWS::Logs::LogGroup`

Represents a log group.

## Argument Reference

### `deletion_protection_enabled`

- **Type:** Bool
- **Required:** No

Use this parameter to enable deletion protection for the new log group. When enabled on a log group, deletion protection blocks all deletion operations until it is explicitly disabled. By default log groups are created without deletion protection enabled.

### `kms_key_id`

- **Type:** String
- **Required:** No

The Amazon Resource Name (ARN) of the KMS key to use when encrypting log data. For more information, see Amazon Resource Names.

### `log_group_class`

- **Type:** [Enum (logGroupClass)](#log_group_class-loggroupclass)
- **Required:** No

Use this parameter to specify the log group class for this log group. There are three classes: The Standard log class supports all CloudWatch Logs features. The Infrequent Access log class supports a subset of CloudWatch Logs features and incurs lower costs. Use the Delivery log class only for delivering Lambda logs to store in Amazon S3 or Amazon Data Firehose. Log events in log groups in the Delivery class are kept in CloudWatch Logs for only one day. This log class doesn't offer rich CloudWatch Logs capabilities such as CloudWatch Logs Insights queries. If you omit this parameter, the default of STANDARD is used. The value of logGroupClass can't be changed after a log group is created. For details about the features supported by each class, see Log classes

### `log_group_name`

- **Type:** String
- **Required:** Yes

A name for the log group.

### `tags`

- **Type:** Map
- **Required:** No

The key-value pairs to use for the tags. You can grant users access to certain log groups while preventing them from accessing other log groups. To do so, tag your groups and use IAM policies that refer to those tags. To assign tags when you create a log group, you must have either the logs:TagResource or logs:TagLogGroup permission. For more information about tagging, see Tagging Amazon Web Services resources. For more information about using tags to control access, see Controlling access to Amazon Web Services resources using tags.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### log_group_class (logGroupClass)

| Value | DSL Identifier |
|-------|----------------|
| `DELIVERY` | `aws.logs.log_group.logGroupClass.DELIVERY` |
| `INFREQUENT_ACCESS` | `aws.logs.log_group.logGroupClass.INFREQUENT_ACCESS` |
| `STANDARD` | `aws.logs.log_group.logGroupClass.STANDARD` |

Shorthand formats: `DELIVERY` or `logGroupClass.DELIVERY`

