//! flow_log schema definition for AWS
//!
//! Auto-generated from Smithy model: com.amazonaws.ec2
//!
//! DO NOT EDIT MANUALLY - regenerate with smithy-codegen

use super::AwsSchemaConfig;
use super::tags_type;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

/// Returns the schema config for ec2.flow_log (Smithy: com.amazonaws.ec2)
pub fn ec2_flow_log_config() -> AwsSchemaConfig {
    AwsSchemaConfig {
        aws_type_name: "AWS::EC2::FlowLog",
        resource_type_name: "ec2.flow_log",
        has_tags: true,
        schema: ResourceSchema::new("aws.ec2.flow_log")
            .with_description("Describes a VPC flow log.")
            .attribute(
                AttributeSchema::new("deliver_logs_permission_arn", super::iam_role_arn())
                    .create_only()
                    .with_description(
                        "The ARN for the IAM role that permits Amazon EC2 to publish flow logs to a CloudWatch Logs log group.",
                    )
                    .with_provider_name("DeliverLogsPermissionArn"),
            )
            .attribute(
                AttributeSchema::new("flow_log_id", super::flow_log_id())
                    .with_description("The ID of the flow log. (read-only)")
                    .with_provider_name("FlowLogId"),
            )
            .attribute(
                AttributeSchema::new("log_destination", super::arn())
                    .create_only()
                    .with_description(
                        "The destination to which the flow log data is to be published.",
                    )
                    .with_provider_name("LogDestination"),
            )
            .attribute(
                AttributeSchema::new(
                    "log_destination_type",
                    AttributeType::StringEnum {
                        name: "LogDestinationType".to_string(),
                        values: vec![
                            "cloud-watch-logs".to_string(),
                            "s3".to_string(),
                            "kinesis-data-firehose".to_string(),
                        ],
                        namespace: Some("aws.ec2.flow_log".to_string()),
                        to_dsl: Some(|s: &str| s.replace('-', "_")),
                    },
                )
                .create_only()
                .with_description(
                    "The type of destination to which the flow log data is to be published.",
                )
                .with_provider_name("LogDestinationType"),
            )
            .attribute(
                AttributeSchema::new("log_format", AttributeType::String)
                    .create_only()
                    .with_description(
                        "The fields to include in the flow log record, in the order in which they should appear.",
                    )
                    .with_provider_name("LogFormat"),
            )
            .attribute(
                AttributeSchema::new("log_group_name", AttributeType::String)
                    .create_only()
                    .with_description(
                        "The name of a new or existing CloudWatch Logs log group where Amazon EC2 publishes your flow logs.",
                    )
                    .with_provider_name("LogGroupName"),
            )
            .attribute(
                AttributeSchema::new("max_aggregation_interval", AttributeType::Int)
                    .create_only()
                    .with_description(
                        "The maximum interval of time during which a flow of packets is captured and aggregated into a flow log record. You can specify 60 seconds (1 minute) or 600 seconds (10 minutes).",
                    )
                    .with_provider_name("MaxAggregationInterval"),
            )
            .attribute(
                AttributeSchema::new("resource_id", AttributeType::String)
                    .required()
                    .create_only()
                    .with_description(
                        "The ID of the subnet, network interface, or VPC for which you want to create a flow log.",
                    )
                    .with_provider_name("ResourceId"),
            )
            .attribute(
                AttributeSchema::new(
                    "resource_type",
                    AttributeType::StringEnum {
                        name: "ResourceType".to_string(),
                        values: vec![
                            "NetworkInterface".to_string(),
                            "Subnet".to_string(),
                            "VPC".to_string(),
                        ],
                        namespace: Some("aws.ec2.flow_log".to_string()),
                        to_dsl: None,
                    },
                )
                .required()
                .create_only()
                .with_description(
                    "The type of resource for which to create the flow log.",
                )
                .with_provider_name("ResourceType"),
            )
            .attribute(
                AttributeSchema::new("tags", tags_type())
                    .with_description("The tags to apply to the flow logs.")
                    .with_provider_name("Tags"),
            )
            .attribute(
                AttributeSchema::new(
                    "traffic_type",
                    AttributeType::StringEnum {
                        name: "TrafficType".to_string(),
                        values: vec![
                            "ACCEPT".to_string(),
                            "ALL".to_string(),
                            "REJECT".to_string(),
                        ],
                        namespace: Some("aws.ec2.flow_log".to_string()),
                        to_dsl: None,
                    },
                )
                .create_only()
                .with_description(
                    "The type of traffic to log. You can log traffic that the resource accepts or rejects, or all traffic.",
                )
                .with_provider_name("TrafficType"),
            ),
    }
}

const VALID_LOG_DESTINATION_TYPE: &[&str] = &["cloud-watch-logs", "s3", "kinesis-data-firehose"];
const VALID_RESOURCE_TYPE: &[&str] = &["NetworkInterface", "Subnet", "VPC"];
const VALID_TRAFFIC_TYPE: &[&str] = &["ACCEPT", "ALL", "REJECT"];

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
pub fn enum_alias_reverse(attr_name: &str, value: &str) -> Option<&'static str> {
    let _ = (attr_name, value);
    None
}
