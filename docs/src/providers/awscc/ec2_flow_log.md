# awscc.ec2_flow_log

CloudFormation Type: `AWS::EC2::FlowLog`

Specifies a VPC flow log, which enables you to capture IP traffic for a specific network interface, subnet, or VPC.

## Attributes

### `deliver_cross_account_role`

- **Type:** String
- **Required:** No

The ARN of the IAM role that allows Amazon EC2 to publish flow logs across accounts.

### `deliver_logs_permission_arn`

- **Type:** String
- **Required:** No

The ARN for the IAM role that permits Amazon EC2 to publish flow logs to a CloudWatch Logs log group in your account. If you specify LogDestinationType as s3 or kinesis-data-firehose, do not specify DeliverLogsPermissionArn or LogGroupName.

### `destination_options`

- **Type:** Map
- **Required:** No

### `id`

- **Type:** String
- **Read-only**

### `log_destination`

- **Type:** String
- **Required:** No

Specifies the destination to which the flow log data is to be published. Flow log data can be published to a CloudWatch Logs log group, an Amazon S3 bucket, or a Kinesis Firehose stream. The value specified for this parameter depends on the value specified for LogDestinationType.

### `log_destination_type`

- **Type:** Enum (LogDestinationType)
- **Required:** No

Specifies the type of destination to which the flow log data is to be published. Flow log data can be published to CloudWatch Logs or Amazon S3.

### `log_format`

- **Type:** String
- **Required:** No

The fields to include in the flow log record, in the order in which they should appear.

### `log_group_name`

- **Type:** String
- **Required:** No

The name of a new or existing CloudWatch Logs log group where Amazon EC2 publishes your flow logs. If you specify LogDestinationType as s3 or kinesis-data-firehose, do not specify DeliverLogsPermissionArn or LogGroupName.

### `max_aggregation_interval`

- **Type:** Int
- **Required:** No

The maximum interval of time during which a flow of packets is captured and aggregated into a flow log record. You can specify 60 seconds (1 minute) or 600 seconds (10 minutes).

### `resource_id`

- **Type:** String
- **Required:** Yes

The ID of the subnet, network interface, or VPC for which you want to create a flow log.

### `resource_type`

- **Type:** Enum (ResourceType)
- **Required:** Yes

The type of resource for which to create the flow log. For example, if you specified a VPC ID for the ResourceId property, specify VPC for this property.

### `tags`

- **Type:** Map
- **Required:** No

The tags to apply to the flow logs.

### `traffic_type`

- **Type:** Enum (TrafficType)
- **Required:** No

The type of traffic to log. You can log traffic that the resource accepts or rejects, or all traffic.

## Enum Values

### log_destination_type (LogDestinationType)

| Value | DSL Identifier |
|-------|----------------|
| `cloud-watch-logs` | `awscc.ec2_flow_log.LogDestinationType.cloud-watch-logs` |
| `s3` | `awscc.ec2_flow_log.LogDestinationType.s3` |
| `kinesis-data-firehose` | `awscc.ec2_flow_log.LogDestinationType.kinesis-data-firehose` |

Shorthand formats: `cloud-watch-logs` or `LogDestinationType.cloud-watch-logs`

### resource_type (ResourceType)

| Value | DSL Identifier |
|-------|----------------|
| `NetworkInterface` | `awscc.ec2_flow_log.ResourceType.NetworkInterface` |
| `Subnet` | `awscc.ec2_flow_log.ResourceType.Subnet` |
| `VPC` | `awscc.ec2_flow_log.ResourceType.VPC` |
| `TransitGateway` | `awscc.ec2_flow_log.ResourceType.TransitGateway` |
| `TransitGatewayAttachment` | `awscc.ec2_flow_log.ResourceType.TransitGatewayAttachment` |
| `RegionalNatGateway` | `awscc.ec2_flow_log.ResourceType.RegionalNatGateway` |

Shorthand formats: `NetworkInterface` or `ResourceType.NetworkInterface`

### traffic_type (TrafficType)

| Value | DSL Identifier |
|-------|----------------|
| `ACCEPT` | `awscc.ec2_flow_log.TrafficType.ACCEPT` |
| `ALL` | `awscc.ec2_flow_log.TrafficType.ALL` |
| `REJECT` | `awscc.ec2_flow_log.TrafficType.REJECT` |

Shorthand formats: `ACCEPT` or `TrafficType.ACCEPT`



## Example

```crn
let vpc = awscc.ec2_vpc {
  name       = "example-vpc"
  cidr_block = "10.0.0.0/16"
}

awscc.ec2_flow_log {
  name                 = "example-flow-log"
  resource_id          = vpc.vpc_id
  resource_type        = VPC
  traffic_type         = ALL
  log_destination_type = s3
  log_destination      = "arn:aws:s3:::example-flow-logs-bucket"

  tags = {
    Environment = "example"
  }
}
```
