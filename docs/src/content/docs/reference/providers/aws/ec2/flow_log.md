---
title: "aws.ec2.FlowLog"
description: "AWS EC2 flow_log resource reference"
---


CloudFormation Type: `AWS::EC2::FlowLog`

Describes a flow log.

## Argument Reference

### `deliver_logs_permission_arn`

- **Type:** IamRoleArn
- **Required:** No

The ARN of the IAM role that allows Amazon EC2 to publish flow logs to the log destination. This parameter is required if the destination type is cloud-watch-logs, or if the destination type is kinesis-data-firehose and the delivery stream and the resources to monitor are in different accounts.

### `log_destination`

- **Type:** Arn
- **Required:** No

The destination for the flow log data. The meaning of this parameter depends on the destination type. If the destination type is cloud-watch-logs, specify the ARN of a CloudWatch Logs log group. For example: arn:aws:logs:region:account_id:log-group:my_group Alternatively, use the LogGroupName parameter. If the destination type is s3, specify the ARN of an S3 bucket. For example: arn:aws:s3:::my_bucket/my_subfolder/ The subfolder is optional. Note that you can't use AWSLogs as a subfolder name. If the destination type is kinesis-data-firehose, specify the ARN of a Kinesis Data Firehose delivery stream. For example: arn:aws:firehose:region:account_id:deliverystream:my_stream

### `log_destination_type`

- **Type:** [Enum (LogDestinationType)](#log_destination_type-logdestinationtype)
- **Required:** No

The type of destination for the flow log data. Default: cloud-watch-logs

### `log_format`

- **Type:** String
- **Required:** No

The fields to include in the flow log record. List the fields in the order in which they should appear. If you omit this parameter, the flow log is created using the default format. If you specify this parameter, you must include at least one field. For more information about the available fields, see Flow log records in the Amazon VPC User Guide or Transit Gateway Flow Log records in the Amazon Web Services Transit Gateway Guide. Specify the fields using the ${field-id} format, separated by spaces.

### `log_group_name`

- **Type:** String
- **Required:** No

The name of a new or existing CloudWatch Logs log group where Amazon EC2 publishes your flow logs. This parameter is valid only if the destination type is cloud-watch-logs.

### `max_aggregation_interval`

- **Type:** Int
- **Required:** No

The maximum interval of time during which a flow of packets is captured and aggregated into a flow log record. The possible values are 60 seconds (1 minute) or 600 seconds (10 minutes). This parameter must be 60 seconds for transit gateway resource types. When a network interface is attached to a Nitro-based instance, the aggregation interval is always 60 seconds or less, regardless of the value that you specify. Default: 600

### `resource_ids`

- **Type:** `List<String>`
- **Required:** Yes

The IDs of the resources to monitor. For example, if the resource type is VPC, specify the IDs of the VPCs. Constraints: Maximum of 25 for transit gateway resource types. Maximum of 1000 for the other resource types.

### `resource_type`

- **Type:** [Enum (ResourceType)](#resource_type-resourcetype)
- **Required:** Yes

The type of resource to monitor.

### `traffic_type`

- **Type:** [Enum (TrafficType)](#traffic_type-traffictype)
- **Required:** No

The type of traffic to monitor (accepted traffic, rejected traffic, or all traffic). This parameter is not supported for transit gateway resource types. It is required for the other resource types.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### log_destination_type (LogDestinationType)

| Value | DSL Identifier |
|-------|----------------|
| `cloud-watch-logs` | `aws.ec2.FlowLog.LogDestinationType.cloud_watch_logs` |
| `kinesis-data-firehose` | `aws.ec2.FlowLog.LogDestinationType.kinesis_data_firehose` |
| `s3` | `aws.ec2.FlowLog.LogDestinationType.s3` |

Shorthand formats: `cloud_watch_logs` or `LogDestinationType.cloud_watch_logs`

### resource_type (ResourceType)

| Value | DSL Identifier |
|-------|----------------|
| `NetworkInterface` | `aws.ec2.FlowLog.ResourceType.NetworkInterface` |
| `RegionalNatGateway` | `aws.ec2.FlowLog.ResourceType.RegionalNatGateway` |
| `Subnet` | `aws.ec2.FlowLog.ResourceType.Subnet` |
| `TransitGateway` | `aws.ec2.FlowLog.ResourceType.TransitGateway` |
| `TransitGatewayAttachment` | `aws.ec2.FlowLog.ResourceType.TransitGatewayAttachment` |
| `VPC` | `aws.ec2.FlowLog.ResourceType.VPC` |

Shorthand formats: `NetworkInterface` or `ResourceType.NetworkInterface`

### traffic_type (TrafficType)

| Value | DSL Identifier |
|-------|----------------|
| `ACCEPT` | `aws.ec2.FlowLog.TrafficType.ACCEPT` |
| `ALL` | `aws.ec2.FlowLog.TrafficType.ALL` |
| `REJECT` | `aws.ec2.FlowLog.TrafficType.REJECT` |

Shorthand formats: `ACCEPT` or `TrafficType.ACCEPT`

## Attribute Reference

### `flow_log_id`

- **Type:** String

