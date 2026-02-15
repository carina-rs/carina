# awscc.s3_bucket

CloudFormation Type: `AWS::S3::Bucket`

The ``AWS::S3::Bucket`` resource creates an Amazon S3 bucket in the same AWS Region where you create the AWS CloudFormation stack.
 To control how AWS CloudFormation handles the bucket when the stack is deleted, you can set a deletion policy for your bucket. You can choose to *retain* the bucket or to *delete* the bucket. For more information, see [DeletionPolicy Attribute](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-attribute-deletionpolicy.html).
  You can only delete empty buckets. Deletion fails for buckets that have contents.

## Argument Reference

### `abac_status`

- **Type:** [Enum (AbacStatus)](#abac_status-abacstatus)
- **Required:** No

The ABAC status of the general purpose bucket. When ABAC is enabled for the general purpose bucket, you can use tags to manage access to the general purpose buckets as well as for cost tracking purposes. When ABAC is disabled for the general purpose buckets, you can only use tags for cost tracking purposes. For more information, see [Using tags with S3 general purpose buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/buckets-tagging.html).

### `accelerate_configuration`

- **Type:** [Struct(AccelerateConfiguration)](#accelerateconfiguration)
- **Required:** No

Configures the transfer acceleration state for an Amazon S3 bucket. For more information, see [Amazon S3 Transfer Acceleration](https://docs.aws.amazon.com/AmazonS3/latest/dev/transfer-acceleration.html) in the *Amazon S3 User Guide*.

### `access_control`

- **Type:** [Enum (AccessControl)](#access_control-accesscontrol)
- **Required:** No

This is a legacy property, and it is not recommended for most use cases. A majority of modern use cases in Amazon S3 no longer require the use of ACLs, and we recommend that you keep ACLs disabled. For more information, see [Controlling object ownership](https://docs.aws.amazon.com//AmazonS3/latest/userguide/about-object-ownership.html) in the *Amazon S3 User Guide*.  A canned access control list (ACL) that grants predefined permissions to the bucket. For more information about canned ACLs, see [Canned ACL](https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#canned-acl) in the *Amazon S3 User Guide*.  S3 buckets are created with ACLs disabled by default. Therefore, unless you explicitly set the [AWS::S3::OwnershipControls](https://docs.aws.amazon.com//AWSCloudFormation/latest/UserGuide/aws-properties-s3-bucket-ownershipcontrols.html) property to enable ACLs, your resource will fail to deploy with any value other than Private. Use cases requiring ACLs are uncommon.  The majority of access control configurations can be successfully and more easily achieved with bucket policies. For more information, see [AWS::S3::BucketPolicy](https://docs.aws.amazon.com//AWSCloudFormation/latest/UserGuide/aws-properties-s3-policy.html). For examples of common policy configurations, including S3 Server Access Logs buckets and more, see [Bucket policy examples](https://docs.aws.amazon.com/AmazonS3/latest/userguide/example-bucket-policies.html) in the *Amazon S3 User Guide*.

### `analytics_configurations`

- **Type:** [List\<AnalyticsConfiguration\>](#analyticsconfiguration)
- **Required:** No

Specifies the configuration and any analyses for the analytics filter of an Amazon S3 bucket.

### `bucket_encryption`

- **Type:** [Struct(BucketEncryption)](#bucketencryption)
- **Required:** No

Specifies default encryption for a bucket using server-side encryption with Amazon S3-managed keys (SSE-S3), AWS KMS-managed keys (SSE-KMS), or dual-layer server-side encryption with KMS-managed keys (DSSE-KMS). For information about the Amazon S3 default encryption feature, see [Amazon S3 Default Encryption for S3 Buckets](https://docs.aws.amazon.com/AmazonS3/latest/dev/bucket-encryption.html) in the *Amazon S3 User Guide*.

### `bucket_name`

- **Type:** String
- **Required:** No

A name for the bucket. If you don't specify a name, AWS CloudFormation generates a unique ID and uses that ID for the bucket name. The bucket name must contain only lowercase letters, numbers, periods (.), and dashes (-) and must follow [Amazon S3 bucket restrictions and limitations](https://docs.aws.amazon.com/AmazonS3/latest/dev/BucketRestrictions.html). For more information, see [Rules for naming Amazon S3 buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html) in the *Amazon S3 User Guide*.  If you specify a name, you can't perform updates that require replacement of this resource. You can perform updates that require no or some interruption. If you need to replace the resource, specify a new name.

### `cors_configuration`

- **Type:** [Struct(CorsConfiguration)](#corsconfiguration)
- **Required:** No

Describes the cross-origin access configuration for objects in an Amazon S3 bucket. For more information, see [Enabling Cross-Origin Resource Sharing](https://docs.aws.amazon.com/AmazonS3/latest/dev/cors.html) in the *Amazon S3 User Guide*.

### `intelligent_tiering_configurations`

- **Type:** [List\<IntelligentTieringConfiguration\>](#intelligenttieringconfiguration)
- **Required:** No

Defines how Amazon S3 handles Intelligent-Tiering storage.

### `inventory_configurations`

- **Type:** [List\<InventoryConfiguration\>](#inventoryconfiguration)
- **Required:** No

Specifies the S3 Inventory configuration for an Amazon S3 bucket. For more information, see [GET Bucket inventory](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucketGETInventoryConfig.html) in the *Amazon S3 API Reference*.

### `lifecycle_configuration`

- **Type:** [Struct(LifecycleConfiguration)](#lifecycleconfiguration)
- **Required:** No

Specifies the lifecycle configuration for objects in an Amazon S3 bucket. For more information, see [Object Lifecycle Management](https://docs.aws.amazon.com/AmazonS3/latest/dev/object-lifecycle-mgmt.html) in the *Amazon S3 User Guide*.

### `logging_configuration`

- **Type:** [Struct(LoggingConfiguration)](#loggingconfiguration)
- **Required:** No

Settings that define where logs are stored.

### `metadata_configuration`

- **Type:** [Struct(MetadataConfiguration)](#metadataconfiguration)
- **Required:** No

The S3 Metadata configuration for a general purpose bucket.

### `metadata_table_configuration`

- **Type:** [Struct(MetadataTableConfiguration)](#metadatatableconfiguration)
- **Required:** No

The metadata table configuration of an S3 general purpose bucket.

### `metrics_configurations`

- **Type:** [List\<MetricsConfiguration\>](#metricsconfiguration)
- **Required:** No

Specifies a metrics configuration for the CloudWatch request metrics (specified by the metrics configuration ID) from an Amazon S3 bucket. If you're updating an existing metrics configuration, note that this is a full replacement of the existing metrics configuration. If you don't include the elements you want to keep, they are erased. For more information, see [PutBucketMetricsConfiguration](https://docs.aws.amazon.com/AmazonS3/latest/API/RESTBucketPUTMetricConfiguration.html).

### `notification_configuration`

- **Type:** [Struct(NotificationConfiguration)](#notificationconfiguration)
- **Required:** No

Configuration that defines how Amazon S3 handles bucket notifications.

### `object_lock_configuration`

- **Type:** [Struct(ObjectLockConfiguration)](#objectlockconfiguration)
- **Required:** No

This operation is not supported for directory buckets.  Places an Object Lock configuration on the specified bucket. The rule specified in the Object Lock configuration will be applied by default to every new object placed in the specified bucket. For more information, see [Locking Objects](https://docs.aws.amazon.com/AmazonS3/latest/dev/object-lock.html).   + The ``DefaultRetention`` settings require both a mode and a period.  + The ``DefaultRetention`` period can be either ``Days`` or ``Years`` but you must select one. You cannot specify ``Days`` and ``Years`` at the same time.  + You can enable Object Lock for new or existing buckets. For more information, see [Configuring Object Lock](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock-configure.html).    You must URL encode any signed header values that contain spaces. For example, if your header value is ``my file.txt``, containing two spaces after ``my``, you must URL encode this value to ``my%20%20file.txt``.

### `object_lock_enabled`

- **Type:** Bool
- **Required:** No

Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled`` when you apply ``ObjectLockConfiguration`` to a bucket.

### `ownership_controls`

- **Type:** [Struct(OwnershipControls)](#ownershipcontrols)
- **Required:** No

Configuration that defines how Amazon S3 handles Object Ownership rules.

### `public_access_block_configuration`

- **Type:** [Struct(PublicAccessBlockConfiguration)](#publicaccessblockconfiguration)
- **Required:** No

Configuration that defines how Amazon S3 handles public access.

### `replication_configuration`

- **Type:** [Struct(ReplicationConfiguration)](#replicationconfiguration)
- **Required:** No

Configuration for replicating objects in an S3 bucket. To enable replication, you must also enable versioning by using the ``VersioningConfiguration`` property. Amazon S3 can store replicated objects in a single destination bucket or multiple destination buckets. The destination bucket or buckets must already exist.

### `tags`

- **Type:** Map
- **Required:** No

An arbitrary set of tags (key-value pairs) for this S3 bucket.

### `versioning_configuration`

- **Type:** [Struct(VersioningConfiguration)](#versioningconfiguration)
- **Required:** No

Enables multiple versions of all objects in this bucket. You might enable versioning to prevent objects from being deleted or overwritten by mistake or to archive objects so that you can retrieve previous versions of them.  When you enable versioning on a bucket for the first time, it might take a short amount of time for the change to be fully propagated. We recommend that you wait for 15 minutes after enabling versioning before issuing write operations (``PUT`` or ``DELETE``) on objects in the bucket.

### `website_configuration`

- **Type:** [Struct(WebsiteConfiguration)](#websiteconfiguration)
- **Required:** No

Information used to configure the bucket as a static website. For more information, see [Hosting Websites on Amazon S3](https://docs.aws.amazon.com/AmazonS3/latest/dev/WebsiteHosting.html).

## Enum Values

### abac_status (AbacStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3_bucket.AbacStatus.Enabled` |
| `Disabled` | `awscc.s3_bucket.AbacStatus.Disabled` |

Shorthand formats: `Enabled` or `AbacStatus.Enabled`

### access_control (AccessControl)

| Value | DSL Identifier |
|-------|----------------|
| `AuthenticatedRead` | `awscc.s3_bucket.AccessControl.AuthenticatedRead` |
| `AwsExecRead` | `awscc.s3_bucket.AccessControl.AwsExecRead` |
| `BucketOwnerFullControl` | `awscc.s3_bucket.AccessControl.BucketOwnerFullControl` |
| `BucketOwnerRead` | `awscc.s3_bucket.AccessControl.BucketOwnerRead` |
| `LogDeliveryWrite` | `awscc.s3_bucket.AccessControl.LogDeliveryWrite` |
| `Private` | `awscc.s3_bucket.AccessControl.Private` |
| `PublicRead` | `awscc.s3_bucket.AccessControl.PublicRead` |
| `PublicReadWrite` | `awscc.s3_bucket.AccessControl.PublicReadWrite` |

Shorthand formats: `AuthenticatedRead` or `AccessControl.AuthenticatedRead`

## Struct Definitions

### AccelerateConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acceleration_status` | String | Yes | Specifies the transfer acceleration status of the bucket. |

### AnalyticsConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | String | Yes | The ID that identifies the analytics configuration. |
| `prefix` | String | No | The prefix that an object must have to be included in the analytics results. |
| `storage_class_analysis` | String | Yes | Contains data related to access patterns to be collected and made available to analyze the tradeoffs... |
| `tag_filters` | `List<String>` | No | The tags to use when evaluating an analytics filter. The analytics only includes objects that meet t... |

### BucketEncryption

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `server_side_encryption_configuration` | `List<String>` | Yes | Specifies the default server-side-encryption configuration. |

### CorsConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cors_rules` | `List<String>` | Yes | A set of origins and methods (cross-origin access that you want to allow). You can add up to 100 rul... |

### IntelligentTieringConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | String | Yes | The ID used to identify the S3 Intelligent-Tiering configuration. |
| `prefix` | String | No | An object key name prefix that identifies the subset of objects to which the rule applies. |
| `status` | String | Yes | Specifies the status of the configuration. |
| `tag_filters` | `List<String>` | No | A container for a key-value pair. |
| `tierings` | `List<String>` | Yes | Specifies a list of S3 Intelligent-Tiering storage class tiers in the configuration. At least one ti... |

### InventoryConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination` | String | Yes | Contains information about where to publish the inventory results. |
| `enabled` | Bool | Yes | Specifies whether the inventory is enabled or disabled. If set to ``True``, an inventory list is gen... |
| `id` | String | Yes | The ID used to identify the inventory configuration. |
| `included_object_versions` | String | Yes | Object versions to include in the inventory list. If set to ``All``, the list includes all the objec... |
| `optional_fields` | `List<String>` | No | Contains the optional fields that are included in the inventory results. |
| `prefix` | String | No | Specifies the inventory filter prefix. |
| `schedule_frequency` | String | Yes | Specifies the schedule for generating inventory results. |

### LifecycleConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rules` | `List<String>` | Yes | A lifecycle rule for individual objects in an Amazon S3 bucket. |
| `transition_default_minimum_object_size` | String | No | Indicates which default minimum object size behavior is applied to the lifecycle configuration.  Thi... |

### LoggingConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination_bucket_name` | String | No | The name of the bucket where Amazon S3 should store server access log files. You can store log files... |
| `log_file_prefix` | String | No | A prefix for all log object keys. If you store log files from multiple Amazon S3 buckets in a single... |
| `target_object_key_format` | String | No | Amazon S3 key format for log objects. Only one format, either PartitionedPrefix or SimplePrefix, is ... |

### MetadataConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination` | String | No | The destination information for the S3 Metadata configuration. |
| `inventory_table_configuration` | String | No | The inventory table configuration for a metadata configuration. |
| `journal_table_configuration` | String | Yes | The journal table configuration for a metadata configuration. |

### MetadataTableConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `s3_tables_destination` | String | Yes | The destination information for the metadata table configuration. The destination table bucket must ... |

### MetricsConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `access_point_arn` | Arn | No | The access point that was used while performing operations on the object. The metrics configuration ... |
| `id` | String | Yes | The ID used to identify the metrics configuration. This can be any value you choose that helps you i... |
| `prefix` | String | No | The prefix that an object must have to be included in the metrics results. |
| `tag_filters` | `List<String>` | No | Specifies a list of tag filters to use as a metrics configuration filter. The metrics configuration ... |

### NotificationConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_bridge_configuration` | String | No | Enables delivery of events to Amazon EventBridge. |
| `lambda_configurations` | `List<String>` | No | Describes the LAMlong functions to invoke and the events for which to invoke them. |
| `queue_configurations` | `List<String>` | No | The Amazon Simple Queue Service queues to publish messages to and the events for which to publish me... |
| `topic_configurations` | `List<String>` | No | The topic to which notifications are sent and the events for which notifications are generated. |

### ObjectLockConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `object_lock_enabled` | String | No | Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled``... |
| `rule` | String | No | Specifies the Object Lock rule for the specified object. Enable this rule when you apply ``ObjectLoc... |

### OwnershipControls

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rules` | `List<String>` | Yes | Specifies the container element for Object Ownership rules. |

### PublicAccessBlockConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `block_public_acls` | Bool | No | Specifies whether Amazon S3 should block public access control lists (ACLs) for this bucket and obje... |
| `block_public_policy` | Bool | No | Specifies whether Amazon S3 should block public bucket policies for this bucket. Setting this elemen... |
| `ignore_public_acls` | Bool | No | Specifies whether Amazon S3 should ignore public ACLs for this bucket and objects in this bucket. Se... |
| `restrict_public_buckets` | Bool | No | Specifies whether Amazon S3 should restrict public bucket policies for this bucket. Setting this ele... |

### ReplicationConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `role` | String | Yes | The Amazon Resource Name (ARN) of the IAMlong (IAM) role that Amazon S3 assumes when replicating obj... |
| `rules` | `List<String>` | Yes | A container for one or more replication rules. A replication configuration must have at least one ru... |

### VersioningConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | String | Yes | The versioning state of the bucket. |

### WebsiteConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `error_document` | String | No | The name of the error document for the website. |
| `index_document` | String | No | The name of the index document for the website. |
| `redirect_all_requests_to` | String | No | The redirect behavior for every request to this bucket's website endpoint.  If you specify this prop... |
| `routing_rules` | `List<String>` | No | Rules that define when a redirect is applied and the redirect behavior. |

## Attribute Reference

### `arn`

- **Type:** Arn

### `domain_name`

- **Type:** String

### `dual_stack_domain_name`

- **Type:** String

### `regional_domain_name`

- **Type:** String

### `website_url`

- **Type:** String


