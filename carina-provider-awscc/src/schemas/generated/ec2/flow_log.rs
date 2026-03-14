//! flow_log schema definition for AWS Cloud Control
//!
//! Auto-generated from CloudFormation schema: AWS::EC2::FlowLog
//!
//! DO NOT EDIT MANUALLY - regenerate with carina-codegen

use super::AwsccSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

const VALID_LOG_DESTINATION_TYPE: &[&str] = &["cloud-watch-logs", "s3", "kinesis-data-firehose"];

const VALID_RESOURCE_TYPE: &[&str] = &[
    "NetworkInterface",
    "Subnet",
    "VPC",
    "TransitGateway",
    "TransitGatewayAttachment",
    "RegionalNatGateway",
];

const VALID_TRAFFIC_TYPE: &[&str] = &["ACCEPT", "ALL", "REJECT"];

/// Returns the schema config for ec2_flow_log (AWS::EC2::FlowLog)
pub fn ec2_flow_log_config() -> AwsccSchemaConfig {
    AwsccSchemaConfig {
        aws_type_name: "AWS::EC2::FlowLog",
        resource_type_name: "ec2.flow_log",
        has_tags: true,
        schema: ResourceSchema::new("awscc.ec2.flow_log")
        .with_description("Specifies a VPC flow log, which enables you to capture IP traffic for a specific network interface, subnet, or VPC.")
        .attribute(
            AttributeSchema::new("deliver_cross_account_role", super::iam_role_arn())
                .create_only()
                .with_description("The ARN of the IAM role that allows Amazon EC2 to publish flow logs across accounts.")
                .with_provider_name("DeliverCrossAccountRole"),
        )
        .attribute(
            AttributeSchema::new("deliver_logs_permission_arn", super::iam_role_arn())
                .create_only()
                .with_description("The ARN for the IAM role that permits Amazon EC2 to publish flow logs to a CloudWatch Logs log group in your account. If you specify LogDestinationTyp...")
                .with_provider_name("DeliverLogsPermissionArn"),
        )
        .attribute(
            AttributeSchema::new("destination_options", AttributeType::Struct {
                    name: "DestinationOptions".to_string(),
                    fields: vec![
                    StructField::new("file_format", AttributeType::StringEnum {
                name: "FileFormat".to_string(),
                values: vec!["plain-text".to_string(), "parquet".to_string()],
                namespace: Some("awscc.ec2.flow_log".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            }).required().with_provider_name("FileFormat"),
                    StructField::new("hive_compatible_partitions", AttributeType::Bool).required().with_provider_name("HiveCompatiblePartitions"),
                    StructField::new("per_hour_partition", AttributeType::Bool).required().with_provider_name("PerHourPartition")
                    ],
                })
                .create_only()
                .with_provider_name("DestinationOptions"),
        )
        .attribute(
            AttributeSchema::new("id", super::flow_log_id())
                .with_description("The Flow Log ID (read-only)")
                .with_provider_name("Id"),
        )
        .attribute(
            AttributeSchema::new("log_destination", super::arn())
                .create_only()
                .with_description("Specifies the destination to which the flow log data is to be published. Flow log data can be published to a CloudWatch Logs log group, an Amazon S3 b...")
                .with_provider_name("LogDestination"),
        )
        .attribute(
            AttributeSchema::new("log_destination_type", AttributeType::StringEnum {
                name: "LogDestinationType".to_string(),
                values: vec!["cloud-watch-logs".to_string(), "s3".to_string(), "kinesis-data-firehose".to_string()],
                namespace: Some("awscc.ec2.flow_log".to_string()),
                to_dsl: Some(|s: &str| s.replace('-', "_")),
            })
                .create_only()
                .with_description("Specifies the type of destination to which the flow log data is to be published. Flow log data can be published to CloudWatch Logs or Amazon S3.")
                .with_provider_name("LogDestinationType"),
        )
        .attribute(
            AttributeSchema::new("log_format", AttributeType::String)
                .create_only()
                .with_description("The fields to include in the flow log record, in the order in which they should appear.")
                .with_provider_name("LogFormat"),
        )
        .attribute(
            AttributeSchema::new("log_group_name", AttributeType::String)
                .create_only()
                .with_description("The name of a new or existing CloudWatch Logs log group where Amazon EC2 publishes your flow logs. If you specify LogDestinationType as s3 or kinesis-...")
                .with_provider_name("LogGroupName"),
        )
        .attribute(
            AttributeSchema::new("max_aggregation_interval", AttributeType::Int)
                .create_only()
                .with_description("The maximum interval of time during which a flow of packets is captured and aggregated into a flow log record. You can specify 60 seconds (1 minute) o...")
                .with_provider_name("MaxAggregationInterval"),
        )
        .attribute(
            AttributeSchema::new("resource_id", AttributeType::String)
                .required()
                .create_only()
                .with_description("The ID of the subnet, network interface, or VPC for which you want to create a flow log.")
                .with_provider_name("ResourceId"),
        )
        .attribute(
            AttributeSchema::new("resource_type", AttributeType::StringEnum {
                name: "ResourceType".to_string(),
                values: vec!["NetworkInterface".to_string(), "Subnet".to_string(), "VPC".to_string(), "TransitGateway".to_string(), "TransitGatewayAttachment".to_string(), "RegionalNatGateway".to_string()],
                namespace: Some("awscc.ec2.flow_log".to_string()),
                to_dsl: None,
            })
                .required()
                .create_only()
                .with_description("The type of resource for which to create the flow log. For example, if you specified a VPC ID for the ResourceId property, specify VPC for this proper...")
                .with_provider_name("ResourceType"),
        )
        .attribute(
            AttributeSchema::new("tags", tags_type())
                .with_description("The tags to apply to the flow logs.")
                .with_provider_name("Tags"),
        )
        .attribute(
            AttributeSchema::new("traffic_type", AttributeType::StringEnum {
                name: "TrafficType".to_string(),
                values: vec!["ACCEPT".to_string(), "ALL".to_string(), "REJECT".to_string()],
                namespace: Some("awscc.ec2.flow_log".to_string()),
                to_dsl: None,
            })
                .create_only()
                .with_description("The type of traffic to log. You can log traffic that the resource accepts or rejects, or all traffic.")
                .with_provider_name("TrafficType"),
        )
    }
}

/// Returns the resource type name and all enum valid values for this module
pub fn enum_valid_values() -> (
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
) {
    (
        "ec2.flow_log",
        &[
            ("log_destination_type", VALID_LOG_DESTINATION_TYPE),
            ("resource_type", VALID_RESOURCE_TYPE),
            ("traffic_type", VALID_TRAFFIC_TYPE),
        ],
    )
}

/// Maps DSL alias values back to canonical AWS values for this module.
/// e.g., ("ip_protocol", "all") -> Some("-1")
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
