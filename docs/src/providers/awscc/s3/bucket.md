# awscc.s3.bucket

CloudFormation Type: `AWS::S3::Bucket`

The ``AWS::S3::Bucket`` resource creates an Amazon S3 bucket in the same AWS Region where you create the AWS CloudFormation stack.
 To control how AWS CloudFormation handles the bucket when the stack is deleted, you can set a deletion policy for your bucket. You can choose to *retain* the bucket or to *delete* the bucket. For more information, see [DeletionPolicy Attribute](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-attribute-deletionpolicy.html).
  You can only delete empty buckets. Deletion fails for buckets that have contents.

## Example

```crn
awscc.s3.bucket {
  bucket_name = "my-example-bucket"

  versioning_configuration = {
    status = Enabled
  }

  tags = {
    Environment = "example"
  }
}
```

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

This is a legacy property, and it is not recommended for most use cases. A majority of modern use cases in Amazon S3 no longer require the use of ACLs, and we recommend that you keep ACLs disabled. For more information, see [Controlling object ownership](https://docs.aws.amazon.com//AmazonS3/latest/userguide/about-object-ownership.html) in the *Amazon S3 User Guide*. A canned access control list (ACL) that grants predefined permissions to the bucket. For more information about canned ACLs, see [Canned ACL](https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#canned-acl) in the *Amazon S3 User Guide*. S3 buckets are created with ACLs disabled by default. Therefore, unless you explicitly set the [AWS::S3::OwnershipControls](https://docs.aws.amazon.com//AWSCloudFormation/latest/UserGuide/aws-properties-s3-bucket-ownershipcontrols.html) property to enable ACLs, your resource will fail to deploy with any value other than Private. Use cases requiring ACLs are uncommon. The majority of access control configurations can be successfully and more easily achieved with bucket policies. For more information, see [AWS::S3::BucketPolicy](https://docs.aws.amazon.com//AWSCloudFormation/latest/UserGuide/aws-properties-s3-policy.html). For examples of common policy configurations, including S3 Server Access Logs buckets and more, see [Bucket policy examples](https://docs.aws.amazon.com/AmazonS3/latest/userguide/example-bucket-policies.html) in the *Amazon S3 User Guide*.

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

A name for the bucket. If you don't specify a name, AWS CloudFormation generates a unique ID and uses that ID for the bucket name. The bucket name must contain only lowercase letters, numbers, periods (.), and dashes (-) and must follow [Amazon S3 bucket restrictions and limitations](https://docs.aws.amazon.com/AmazonS3/latest/dev/BucketRestrictions.html). For more information, see [Rules for naming Amazon S3 buckets](https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html) in the *Amazon S3 User Guide*. If you specify a name, you can't perform updates that require replacement of this resource. You can perform updates that require no or some interruption. If you need to replace the resource, specify a new name.

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

This operation is not supported for directory buckets. Places an Object Lock configuration on the specified bucket. The rule specified in the Object Lock configuration will be applied by default to every new object placed in the specified bucket. For more information, see [Locking Objects](https://docs.aws.amazon.com/AmazonS3/latest/dev/object-lock.html). + The ``DefaultRetention`` settings require both a mode and a period. + The ``DefaultRetention`` period can be either ``Days`` or ``Years`` but you must select one. You cannot specify ``Days`` and ``Years`` at the same time. + You can enable Object Lock for new or existing buckets. For more information, see [Configuring Object Lock](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock-configure.html). You must URL encode any signed header values that contain spaces. For example, if your header value is ``my file.txt``, containing two spaces after ``my``, you must URL encode this value to ``my%20%20file.txt``.

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

Enables multiple versions of all objects in this bucket. You might enable versioning to prevent objects from being deleted or overwritten by mistake or to archive objects so that you can retrieve previous versions of them. When you enable versioning on a bucket for the first time, it might take a short amount of time for the change to be fully propagated. We recommend that you wait for 15 minutes after enabling versioning before issuing write operations (``PUT`` or ``DELETE``) on objects in the bucket.

### `website_configuration`

- **Type:** [Struct(WebsiteConfiguration)](#websiteconfiguration)
- **Required:** No

Information used to configure the bucket as a static website. For more information, see [Hosting Websites on Amazon S3](https://docs.aws.amazon.com/AmazonS3/latest/dev/WebsiteHosting.html).

## Enum Values

### abac_status (AbacStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3.bucket.AbacStatus.Enabled` |
| `Disabled` | `awscc.s3.bucket.AbacStatus.Disabled` |

Shorthand formats: `Enabled` or `AbacStatus.Enabled`

### acceleration_status (AccelerationStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3.bucket.AccelerationStatus.Enabled` |
| `Suspended` | `awscc.s3.bucket.AccelerationStatus.Suspended` |

Shorthand formats: `Enabled` or `AccelerationStatus.Enabled`

### access_control (AccessControl)

| Value | DSL Identifier |
|-------|----------------|
| `AuthenticatedRead` | `awscc.s3.bucket.AccessControl.AuthenticatedRead` |
| `AwsExecRead` | `awscc.s3.bucket.AccessControl.AwsExecRead` |
| `BucketOwnerFullControl` | `awscc.s3.bucket.AccessControl.BucketOwnerFullControl` |
| `BucketOwnerRead` | `awscc.s3.bucket.AccessControl.BucketOwnerRead` |
| `LogDeliveryWrite` | `awscc.s3.bucket.AccessControl.LogDeliveryWrite` |
| `Private` | `awscc.s3.bucket.AccessControl.Private` |
| `PublicRead` | `awscc.s3.bucket.AccessControl.PublicRead` |
| `PublicReadWrite` | `awscc.s3.bucket.AccessControl.PublicReadWrite` |

Shorthand formats: `AuthenticatedRead` or `AccessControl.AuthenticatedRead`

### owner (Owner)

| Value | DSL Identifier |
|-------|----------------|
| `Destination` | `awscc.s3.bucket.Owner.Destination` |

Shorthand formats: `Destination` or `Owner.Destination`

### encryption_type (EncryptionType)

| Value | DSL Identifier |
|-------|----------------|
| `NONE` | `awscc.s3.bucket.EncryptionType.NONE` |
| `SSE-C` | `awscc.s3.bucket.EncryptionType.SSE_C` |

Shorthand formats: `NONE` or `EncryptionType.NONE`

### allowed_methods (AllowedMethods)

| Value | DSL Identifier |
|-------|----------------|
| `GET` | `awscc.s3.bucket.AllowedMethods.GET` |
| `PUT` | `awscc.s3.bucket.AllowedMethods.PUT` |
| `HEAD` | `awscc.s3.bucket.AllowedMethods.HEAD` |
| `POST` | `awscc.s3.bucket.AllowedMethods.POST` |
| `DELETE` | `awscc.s3.bucket.AllowedMethods.DELETE` |

Shorthand formats: `GET` or `AllowedMethods.GET`

### output_schema_version (OutputSchemaVersion)

| Value | DSL Identifier |
|-------|----------------|
| `V_1` | `awscc.s3.bucket.OutputSchemaVersion.V_1` |

Shorthand formats: `V_1` or `OutputSchemaVersion.V_1`

### mode (Mode)

| Value | DSL Identifier |
|-------|----------------|
| `COMPLIANCE` | `awscc.s3.bucket.Mode.COMPLIANCE` |
| `GOVERNANCE` | `awscc.s3.bucket.Mode.GOVERNANCE` |

Shorthand formats: `COMPLIANCE` or `Mode.COMPLIANCE`

### status (DeleteMarkerReplicationStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Disabled` | `awscc.s3.bucket.DeleteMarkerReplicationStatus.Disabled` |
| `Enabled` | `awscc.s3.bucket.DeleteMarkerReplicationStatus.Enabled` |

Shorthand formats: `Disabled` or `DeleteMarkerReplicationStatus.Disabled`

### format (Format)

| Value | DSL Identifier |
|-------|----------------|
| `CSV` | `awscc.s3.bucket.Format.CSV` |
| `ORC` | `awscc.s3.bucket.Format.ORC` |
| `Parquet` | `awscc.s3.bucket.Format.Parquet` |

Shorthand formats: `CSV` or `Format.CSV`

### status (IntelligentTieringConfigurationStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Disabled` | `awscc.s3.bucket.IntelligentTieringConfigurationStatus.Disabled` |
| `Enabled` | `awscc.s3.bucket.IntelligentTieringConfigurationStatus.Enabled` |

Shorthand formats: `Disabled` or `IntelligentTieringConfigurationStatus.Disabled`

### included_object_versions (IncludedObjectVersions)

| Value | DSL Identifier |
|-------|----------------|
| `All` | `awscc.s3.bucket.IncludedObjectVersions.All` |
| `Current` | `awscc.s3.bucket.IncludedObjectVersions.Current` |

Shorthand formats: `All` or `IncludedObjectVersions.All`

### optional_fields (OptionalFields)

| Value | DSL Identifier |
|-------|----------------|
| `Size` | `awscc.s3.bucket.OptionalFields.Size` |
| `LastModifiedDate` | `awscc.s3.bucket.OptionalFields.LastModifiedDate` |
| `StorageClass` | `awscc.s3.bucket.OptionalFields.StorageClass` |
| `ETag` | `awscc.s3.bucket.OptionalFields.ETag` |
| `IsMultipartUploaded` | `awscc.s3.bucket.OptionalFields.IsMultipartUploaded` |
| `ReplicationStatus` | `awscc.s3.bucket.OptionalFields.ReplicationStatus` |
| `EncryptionStatus` | `awscc.s3.bucket.OptionalFields.EncryptionStatus` |
| `ObjectLockRetainUntilDate` | `awscc.s3.bucket.OptionalFields.ObjectLockRetainUntilDate` |
| `ObjectLockMode` | `awscc.s3.bucket.OptionalFields.ObjectLockMode` |
| `ObjectLockLegalHoldStatus` | `awscc.s3.bucket.OptionalFields.ObjectLockLegalHoldStatus` |
| `IntelligentTieringAccessTier` | `awscc.s3.bucket.OptionalFields.IntelligentTieringAccessTier` |
| `BucketKeyStatus` | `awscc.s3.bucket.OptionalFields.BucketKeyStatus` |
| `ChecksumAlgorithm` | `awscc.s3.bucket.OptionalFields.ChecksumAlgorithm` |
| `ObjectAccessControlList` | `awscc.s3.bucket.OptionalFields.ObjectAccessControlList` |
| `ObjectOwner` | `awscc.s3.bucket.OptionalFields.ObjectOwner` |
| `LifecycleExpirationDate` | `awscc.s3.bucket.OptionalFields.LifecycleExpirationDate` |

Shorthand formats: `Size` or `OptionalFields.Size`

### schedule_frequency (ScheduleFrequency)

| Value | DSL Identifier |
|-------|----------------|
| `Daily` | `awscc.s3.bucket.ScheduleFrequency.Daily` |
| `Weekly` | `awscc.s3.bucket.ScheduleFrequency.Weekly` |

Shorthand formats: `Daily` or `ScheduleFrequency.Daily`

### configuration_state (ConfigurationState)

| Value | DSL Identifier |
|-------|----------------|
| `ENABLED` | `awscc.s3.bucket.ConfigurationState.ENABLED` |
| `DISABLED` | `awscc.s3.bucket.ConfigurationState.DISABLED` |

Shorthand formats: `ENABLED` or `ConfigurationState.ENABLED`

### transition_default_minimum_object_size (TransitionDefaultMinimumObjectSize)

| Value | DSL Identifier |
|-------|----------------|
| `varies_by_storage_class` | `awscc.s3.bucket.TransitionDefaultMinimumObjectSize.varies_by_storage_class` |
| `all_storage_classes_128K` | `awscc.s3.bucket.TransitionDefaultMinimumObjectSize.all_storage_classes_128K` |

Shorthand formats: `varies_by_storage_class` or `TransitionDefaultMinimumObjectSize.varies_by_storage_class`

### table_bucket_type (TableBucketType)

| Value | DSL Identifier |
|-------|----------------|
| `aws` | `awscc.s3.bucket.TableBucketType.aws` |
| `customer` | `awscc.s3.bucket.TableBucketType.customer` |

Shorthand formats: `aws` or `TableBucketType.aws`

### sse_algorithm (MetadataTableEncryptionConfigurationSseAlgorithm)

| Value | DSL Identifier |
|-------|----------------|
| `aws:kms` | `awscc.s3.bucket.MetadataTableEncryptionConfigurationSseAlgorithm.aws:kms` |
| `AES256` | `awscc.s3.bucket.MetadataTableEncryptionConfigurationSseAlgorithm.AES256` |

Shorthand formats: `aws:kms` or `MetadataTableEncryptionConfigurationSseAlgorithm.aws:kms`

### status (MetricsStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Disabled` | `awscc.s3.bucket.MetricsStatus.Disabled` |
| `Enabled` | `awscc.s3.bucket.MetricsStatus.Enabled` |

Shorthand formats: `Disabled` or `MetricsStatus.Disabled`

### storage_class (NoncurrentVersionTransitionStorageClass)

| Value | DSL Identifier |
|-------|----------------|
| `DEEP_ARCHIVE` | `awscc.s3.bucket.NoncurrentVersionTransitionStorageClass.DEEP_ARCHIVE` |
| `GLACIER` | `awscc.s3.bucket.NoncurrentVersionTransitionStorageClass.GLACIER` |
| `GLACIER_IR` | `awscc.s3.bucket.NoncurrentVersionTransitionStorageClass.GLACIER_IR` |
| `INTELLIGENT_TIERING` | `awscc.s3.bucket.NoncurrentVersionTransitionStorageClass.INTELLIGENT_TIERING` |
| `ONEZONE_IA` | `awscc.s3.bucket.NoncurrentVersionTransitionStorageClass.ONEZONE_IA` |
| `STANDARD_IA` | `awscc.s3.bucket.NoncurrentVersionTransitionStorageClass.STANDARD_IA` |

Shorthand formats: `DEEP_ARCHIVE` or `NoncurrentVersionTransitionStorageClass.DEEP_ARCHIVE`

### object_lock_enabled (ObjectLockEnabled)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3.bucket.ObjectLockEnabled.Enabled` |

Shorthand formats: `Enabled` or `ObjectLockEnabled.Enabled`

### object_ownership (ObjectOwnership)

| Value | DSL Identifier |
|-------|----------------|
| `ObjectWriter` | `awscc.s3.bucket.ObjectOwnership.ObjectWriter` |
| `BucketOwnerPreferred` | `awscc.s3.bucket.ObjectOwnership.BucketOwnerPreferred` |
| `BucketOwnerEnforced` | `awscc.s3.bucket.ObjectOwnership.BucketOwnerEnforced` |

Shorthand formats: `ObjectWriter` or `ObjectOwnership.ObjectWriter`

### partition_date_source (PartitionDateSource)

| Value | DSL Identifier |
|-------|----------------|
| `EventTime` | `awscc.s3.bucket.PartitionDateSource.EventTime` |
| `DeliveryTime` | `awscc.s3.bucket.PartitionDateSource.DeliveryTime` |

Shorthand formats: `EventTime` or `PartitionDateSource.EventTime`

### expiration (Expiration)

| Value | DSL Identifier |
|-------|----------------|
| `ENABLED` | `awscc.s3.bucket.Expiration.ENABLED` |
| `DISABLED` | `awscc.s3.bucket.Expiration.DISABLED` |

Shorthand formats: `ENABLED` or `Expiration.ENABLED`

### protocol (Protocol)

| Value | DSL Identifier |
|-------|----------------|
| `http` | `awscc.s3.bucket.Protocol.http` |
| `https` | `awscc.s3.bucket.Protocol.https` |

Shorthand formats: `http` or `Protocol.http`

### status (ReplicaModificationsStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3.bucket.ReplicaModificationsStatus.Enabled` |
| `Disabled` | `awscc.s3.bucket.ReplicaModificationsStatus.Disabled` |

Shorthand formats: `Enabled` or `ReplicaModificationsStatus.Enabled`

### storage_class (ReplicationDestinationStorageClass)

| Value | DSL Identifier |
|-------|----------------|
| `DEEP_ARCHIVE` | `awscc.s3.bucket.ReplicationDestinationStorageClass.DEEP_ARCHIVE` |
| `GLACIER` | `awscc.s3.bucket.ReplicationDestinationStorageClass.GLACIER` |
| `GLACIER_IR` | `awscc.s3.bucket.ReplicationDestinationStorageClass.GLACIER_IR` |
| `INTELLIGENT_TIERING` | `awscc.s3.bucket.ReplicationDestinationStorageClass.INTELLIGENT_TIERING` |
| `ONEZONE_IA` | `awscc.s3.bucket.ReplicationDestinationStorageClass.ONEZONE_IA` |
| `REDUCED_REDUNDANCY` | `awscc.s3.bucket.ReplicationDestinationStorageClass.REDUCED_REDUNDANCY` |
| `STANDARD` | `awscc.s3.bucket.ReplicationDestinationStorageClass.STANDARD` |
| `STANDARD_IA` | `awscc.s3.bucket.ReplicationDestinationStorageClass.STANDARD_IA` |

Shorthand formats: `DEEP_ARCHIVE` or `ReplicationDestinationStorageClass.DEEP_ARCHIVE`

### status (ReplicationRuleStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Disabled` | `awscc.s3.bucket.ReplicationRuleStatus.Disabled` |
| `Enabled` | `awscc.s3.bucket.ReplicationRuleStatus.Enabled` |

Shorthand formats: `Disabled` or `ReplicationRuleStatus.Disabled`

### status (ReplicationTimeStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Disabled` | `awscc.s3.bucket.ReplicationTimeStatus.Disabled` |
| `Enabled` | `awscc.s3.bucket.ReplicationTimeStatus.Enabled` |

Shorthand formats: `Disabled` or `ReplicationTimeStatus.Disabled`

### status (RuleStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3.bucket.RuleStatus.Enabled` |
| `Disabled` | `awscc.s3.bucket.RuleStatus.Disabled` |

Shorthand formats: `Enabled` or `RuleStatus.Enabled`

### sse_algorithm (ServerSideEncryptionByDefaultSseAlgorithm)

| Value | DSL Identifier |
|-------|----------------|
| `aws:kms` | `awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.aws:kms` |
| `AES256` | `awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.AES256` |
| `aws:kms:dsse` | `awscc.s3.bucket.ServerSideEncryptionByDefaultSseAlgorithm.aws:kms:dsse` |

Shorthand formats: `aws:kms` or `ServerSideEncryptionByDefaultSseAlgorithm.aws:kms`

### status (SseKmsEncryptedObjectsStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Disabled` | `awscc.s3.bucket.SseKmsEncryptedObjectsStatus.Disabled` |
| `Enabled` | `awscc.s3.bucket.SseKmsEncryptedObjectsStatus.Enabled` |

Shorthand formats: `Disabled` or `SseKmsEncryptedObjectsStatus.Disabled`

### access_tier (AccessTier)

| Value | DSL Identifier |
|-------|----------------|
| `ARCHIVE_ACCESS` | `awscc.s3.bucket.AccessTier.ARCHIVE_ACCESS` |
| `DEEP_ARCHIVE_ACCESS` | `awscc.s3.bucket.AccessTier.DEEP_ARCHIVE_ACCESS` |

Shorthand formats: `ARCHIVE_ACCESS` or `AccessTier.ARCHIVE_ACCESS`

### storage_class (TransitionStorageClass)

| Value | DSL Identifier |
|-------|----------------|
| `DEEP_ARCHIVE` | `awscc.s3.bucket.TransitionStorageClass.DEEP_ARCHIVE` |
| `GLACIER` | `awscc.s3.bucket.TransitionStorageClass.GLACIER` |
| `GLACIER_IR` | `awscc.s3.bucket.TransitionStorageClass.GLACIER_IR` |
| `INTELLIGENT_TIERING` | `awscc.s3.bucket.TransitionStorageClass.INTELLIGENT_TIERING` |
| `ONEZONE_IA` | `awscc.s3.bucket.TransitionStorageClass.ONEZONE_IA` |
| `STANDARD_IA` | `awscc.s3.bucket.TransitionStorageClass.STANDARD_IA` |

Shorthand formats: `DEEP_ARCHIVE` or `TransitionStorageClass.DEEP_ARCHIVE`

### status (VersioningConfigurationStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `awscc.s3.bucket.VersioningConfigurationStatus.Enabled` |
| `Suspended` | `awscc.s3.bucket.VersioningConfigurationStatus.Suspended` |

Shorthand formats: `Enabled` or `VersioningConfigurationStatus.Enabled`

## Struct Definitions

### AbortIncompleteMultipartUpload

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `days_after_initiation` | Int(0..) | Yes | Specifies the number of days after which Amazon S3 stops an incomplete multipart upload. |

### AccelerateConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acceleration_status` | [Enum (AccelerationStatus)](#acceleration_status-accelerationstatus) | Yes | Specifies the transfer acceleration status of the bucket. |

### AccessControlTranslation

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `owner` | [Enum (Owner)](#owner-owner) | Yes | Specifies the replica ownership. For default and valid values, see [PUT bucket replication](https://... |

### AnalyticsConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | String | Yes | The ID that identifies the analytics configuration. |
| `prefix` | String | No | The prefix that an object must have to be included in the analytics results. |
| `storage_class_analysis` | [Struct(StorageClassAnalysis)](#storageclassanalysis) | Yes | Contains data related to access patterns to be collected and made available to analyze the tradeoffs... |
| `tag_filters` | `List<Map>` | No | The tags to use when evaluating an analytics filter. The analytics only includes objects that meet t... |

### BlockedEncryptionTypes

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `encryption_type` | [Enum (EncryptionType)](#encryption_type-encryptiontype) | No | The object encryption type that you want to block or unblock for an Amazon S3 general purpose bucket... |

### BucketEncryption

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `server_side_encryption_configuration` | [List\<ServerSideEncryptionRule\>](#serversideencryptionrule) | Yes | Specifies the default server-side-encryption configuration. |

### CorsConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cors_rules` | [List\<CorsRule\>](#corsrule) | Yes | A set of origins and methods (cross-origin access that you want to allow). You can add up to 100 rul... |

### CorsRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `allowed_headers` | `List<String>` | No | Headers that are specified in the ``Access-Control-Request-Headers`` header. These headers are allow... |
| `allowed_methods` | List\<[Enum (AllowedMethods)](#allowed_methods-allowedmethods)\> | Yes | An HTTP method that you allow the origin to run. *Allowed values*: ``GET`` | ``PUT`` | ``HEAD`` | ``... |
| `allowed_origins` | `List<String>` | Yes | One or more origins you want customers to be able to access the bucket from. |
| `exposed_headers` | `List<String>` | No | One or more headers in the response that you want customers to be able to access from their applicat... |
| `id` | String(len: ..=255) | No | A unique identifier for this rule. The value must be no more than 255 characters. |
| `max_age` | Int(0..) | No | The time in seconds that your browser is to cache the preflight response for the specified resource. |

### DataExport

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination` | [Struct(Destination)](#destination) | Yes | The place to store the data for an analysis. |
| `output_schema_version` | [Enum (OutputSchemaVersion)](#output_schema_version-outputschemaversion) | Yes | The version of the output schema to use when exporting data. Must be ``V_1``. |

### DefaultRetention

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `days` | Int | No | The number of days that you want to specify for the default retention period. If Object Lock is turn... |
| `mode` | [Enum (Mode)](#mode-mode) | No | The default Object Lock retention mode you want to apply to new objects placed in the specified buck... |
| `years` | Int | No | The number of years that you want to specify for the default retention period. If Object Lock is tur... |

### DeleteMarkerReplication

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | [Enum (DeleteMarkerReplicationStatus)](#status-deletemarkerreplicationstatus) | No | Indicates whether to replicate delete markers. |

### Destination

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `bucket_account_id` | AwsAccountId | No | The account ID that owns the destination S3 bucket. If no account ID is provided, the owner is not v... |
| `bucket_arn` | Arn | Yes | The Amazon Resource Name (ARN) of the bucket to which data is exported. |
| `format` | [Enum (Format)](#format-format) | Yes | Specifies the file format used when exporting data to Amazon S3. *Allowed values*: ``CSV`` | ``ORC``... |
| `prefix` | String | No | The prefix to use when exporting data. The prefix is prepended to all results. |

### EncryptionConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `replica_kms_key_id` | KmsKeyId | Yes | Specifies the ID (Key ARN or Alias ARN) of the customer managed AWS KMS key stored in AWS Key Manage... |

### EventBridgeConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_bridge_enabled` | Bool | Yes | Enables delivery of events to Amazon EventBridge. |

### FilterRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | String(len: ..=1024) | Yes | The object key name prefix or suffix identifying one or more objects to which the filtering rule app... |
| `value` | String | Yes | The value that the filter searches for in object key names. |

### IntelligentTieringConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | String | Yes | The ID used to identify the S3 Intelligent-Tiering configuration. |
| `prefix` | String | No | An object key name prefix that identifies the subset of objects to which the rule applies. |
| `status` | [Enum (IntelligentTieringConfigurationStatus)](#status-intelligenttieringconfigurationstatus) | Yes | Specifies the status of the configuration. |
| `tag_filters` | `List<Map>` | No | A container for a key-value pair. |
| `tierings` | [List\<Tiering\>](#tiering) | Yes | Specifies a list of S3 Intelligent-Tiering storage class tiers in the configuration. At least one ti... |

### InventoryConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination` | [Struct(Destination)](#destination) | Yes | Contains information about where to publish the inventory results. |
| `enabled` | Bool | Yes | Specifies whether the inventory is enabled or disabled. If set to ``True``, an inventory list is gen... |
| `id` | String | Yes | The ID used to identify the inventory configuration. |
| `included_object_versions` | [Enum (IncludedObjectVersions)](#included_object_versions-includedobjectversions) | Yes | Object versions to include in the inventory list. If set to ``All``, the list includes all the objec... |
| `optional_fields` | List\<[Enum (OptionalFields)](#optional_fields-optionalfields)\> | No | Contains the optional fields that are included in the inventory results. |
| `prefix` | String | No | Specifies the inventory filter prefix. |
| `schedule_frequency` | [Enum (ScheduleFrequency)](#schedule_frequency-schedulefrequency) | Yes | Specifies the schedule for generating inventory results. |

### InventoryTableConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `configuration_state` | [Enum (ConfigurationState)](#configuration_state-configurationstate) | Yes | The configuration state of the inventory table, indicating whether the inventory table is enabled or... |
| `encryption_configuration` | [Struct(MetadataTableEncryptionConfiguration)](#metadatatableencryptionconfiguration) | No | The encryption configuration for the inventory table. |
| `table_arn` | Arn | No | The Amazon Resource Name (ARN) for the inventory table. |
| `table_name` | String | No | The name of the inventory table. |

### JournalTableConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `encryption_configuration` | [Struct(MetadataTableEncryptionConfiguration)](#metadatatableencryptionconfiguration) | No | The encryption configuration for the journal table. |
| `record_expiration` | [Struct(RecordExpiration)](#recordexpiration) | Yes | The journal table record expiration settings for the journal table. |
| `table_arn` | Arn | No | The Amazon Resource Name (ARN) for the journal table. |
| `table_name` | String | No | The name of the journal table. |

### LambdaConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event` | String | Yes | The Amazon S3 bucket event for which to invoke the LAMlong function. For more information, see [Supp... |
| `filter` | [Struct(NotificationFilter)](#notificationfilter) | No | The filtering rules that determine which objects invoke the AWS Lambda function. For example, you ca... |
| `function` | Arn | Yes | The Amazon Resource Name (ARN) of the LAMlong function that Amazon S3 invokes when the specified eve... |

### LifecycleConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rules` | [List\<Rule\>](#rule) | Yes | A lifecycle rule for individual objects in an Amazon S3 bucket. |
| `transition_default_minimum_object_size` | [Enum (TransitionDefaultMinimumObjectSize)](#transition_default_minimum_object_size-transitiondefaultminimumobjectsize) | No | Indicates which default minimum object size behavior is applied to the lifecycle configuration. This... |

### LoggingConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination_bucket_name` | String | No | The name of the bucket where Amazon S3 should store server access log files. You can store log files... |
| `log_file_prefix` | String | No | A prefix for all log object keys. If you store log files from multiple Amazon S3 buckets in a single... |
| `target_object_key_format` | [Struct(TargetObjectKeyFormat)](#targetobjectkeyformat) | No | Amazon S3 key format for log objects. Only one format, either PartitionedPrefix or SimplePrefix, is ... |

### MetadataConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `destination` | [Struct(MetadataDestination)](#metadatadestination) | No | The destination information for the S3 Metadata configuration. |
| `inventory_table_configuration` | [Struct(InventoryTableConfiguration)](#inventorytableconfiguration) | No | The inventory table configuration for a metadata configuration. |
| `journal_table_configuration` | [Struct(JournalTableConfiguration)](#journaltableconfiguration) | Yes | The journal table configuration for a metadata configuration. |

### MetadataDestination

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `table_bucket_arn` | Arn | No | The Amazon Resource Name (ARN) of the table bucket where the metadata configuration is stored. |
| `table_bucket_type` | [Enum (TableBucketType)](#table_bucket_type-tablebuckettype) | Yes | The type of the table bucket where the metadata configuration is stored. The ``aws`` value indicates... |
| `table_namespace` | String | No | The namespace in the table bucket where the metadata tables for a metadata configuration are stored. |

### MetadataTableConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `s3_tables_destination` | [Struct(S3TablesDestination)](#s3tablesdestination) | Yes | The destination information for the metadata table configuration. The destination table bucket must ... |

### MetadataTableEncryptionConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kms_key_arn` | KmsKeyArn | No | If server-side encryption with KMSlong (KMS) keys (SSE-KMS) is specified, you must also specify the ... |
| `sse_algorithm` | [Enum (MetadataTableEncryptionConfigurationSseAlgorithm)](#sse_algorithm-metadatatableencryptionconfigurationssealgorithm) | Yes | The encryption type specified for a metadata table. To specify server-side encryption with KMSlong (... |

### Metrics

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_threshold` | [Struct(ReplicationTimeValue)](#replicationtimevalue) | No | A container specifying the time threshold for emitting the ``s3:Replication:OperationMissedThreshold... |
| `status` | [Enum (MetricsStatus)](#status-metricsstatus) | Yes | Specifies whether the replication metrics are enabled. |

### MetricsConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `access_point_arn` | Arn | No | The access point that was used while performing operations on the object. The metrics configuration ... |
| `id` | String | Yes | The ID used to identify the metrics configuration. This can be any value you choose that helps you i... |
| `prefix` | String | No | The prefix that an object must have to be included in the metrics results. |
| `tag_filters` | `List<Map>` | No | Specifies a list of tag filters to use as a metrics configuration filter. The metrics configuration ... |

### NoncurrentVersionExpiration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `newer_noncurrent_versions` | Int | No | Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent... |
| `noncurrent_days` | Int | Yes | Specifies the number of days an object is noncurrent before S3 can perform the associated action. Fo... |

### NoncurrentVersionTransition

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `newer_noncurrent_versions` | Int | No | Specifies how many noncurrent versions S3 will retain. If there are this many more recent noncurrent... |
| `storage_class` | [Enum (NoncurrentVersionTransitionStorageClass)](#storage_class-noncurrentversiontransitionstorageclass) | Yes | The class of storage used to store the object. |
| `transition_in_days` | Int | Yes | Specifies the number of days an object is noncurrent before Amazon S3 can perform the associated act... |

### NotificationConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event_bridge_configuration` | [Struct(EventBridgeConfiguration)](#eventbridgeconfiguration) | No | Enables delivery of events to Amazon EventBridge. |
| `lambda_configurations` | [List\<LambdaConfiguration\>](#lambdaconfiguration) | No | Describes the LAMlong functions to invoke and the events for which to invoke them. |
| `queue_configurations` | [List\<QueueConfiguration\>](#queueconfiguration) | No | The Amazon Simple Queue Service queues to publish messages to and the events for which to publish me... |
| `topic_configurations` | [List\<TopicConfiguration\>](#topicconfiguration) | No | The topic to which notifications are sent and the events for which notifications are generated. |

### NotificationFilter

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `s3_key` | [Struct(S3KeyFilter)](#s3keyfilter) | Yes | A container for object key name prefix and suffix filtering rules. |

### ObjectLockConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `object_lock_enabled` | [Enum (ObjectLockEnabled)](#object_lock_enabled-objectlockenabled) | No | Indicates whether this bucket has an Object Lock configuration enabled. Enable ``ObjectLockEnabled``... |
| `rule` | [Struct(ObjectLockRule)](#objectlockrule) | No | Specifies the Object Lock rule for the specified object. Enable this rule when you apply ``ObjectLoc... |

### ObjectLockRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `default_retention` | [Struct(DefaultRetention)](#defaultretention) | No | The default Object Lock retention mode and period that you want to apply to new objects placed in th... |

### OwnershipControls

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rules` | [List\<OwnershipControlsRule\>](#ownershipcontrolsrule) | Yes | Specifies the container element for Object Ownership rules. |

### OwnershipControlsRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `object_ownership` | [Enum (ObjectOwnership)](#object_ownership-objectownership) | No | Specifies an object ownership rule. |

### PartitionedPrefix

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `partition_date_source` | [Enum (PartitionDateSource)](#partition_date_source-partitiondatesource) | No | Specifies the partition date source for the partitioned prefix. ``PartitionDateSource`` can be ``Eve... |

### PublicAccessBlockConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `block_public_acls` | Bool | No | Specifies whether Amazon S3 should block public access control lists (ACLs) for this bucket and obje... |
| `block_public_policy` | Bool | No | Specifies whether Amazon S3 should block public bucket policies for this bucket. Setting this elemen... |
| `ignore_public_acls` | Bool | No | Specifies whether Amazon S3 should ignore public ACLs for this bucket and objects in this bucket. Se... |
| `restrict_public_buckets` | Bool | No | Specifies whether Amazon S3 should restrict public bucket policies for this bucket. Setting this ele... |

### QueueConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event` | String | Yes | The Amazon S3 bucket event about which you want to publish messages to Amazon SQS. For more informat... |
| `filter` | [Struct(NotificationFilter)](#notificationfilter) | No | The filtering rules that determine which objects trigger notifications. For example, you can create ... |
| `queue` | Arn | Yes | The Amazon Resource Name (ARN) of the Amazon SQS queue to which Amazon S3 publishes a message when i... |

### RecordExpiration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `days` | Int | No | If you enable journal table record expiration, you can set the number of days to retain your journal... |
| `expiration` | [Enum (Expiration)](#expiration-expiration) | Yes | Specifies whether journal table record expiration is enabled or disabled. |

### RedirectAllRequestsTo

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `host_name` | String | Yes | Name of the host where requests are redirected. |
| `protocol` | [Enum (Protocol)](#protocol-protocol) | No | Protocol to use when redirecting requests. The default is the protocol that is used in the original ... |

### RedirectRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `host_name` | String | No | The host name to use in the redirect request. |
| `http_redirect_code` | String | No | The HTTP redirect code to use on the response. Not required if one of the siblings is present. |
| `protocol` | [Enum (Protocol)](#protocol-protocol) | No | Protocol to use when redirecting requests. The default is the protocol that is used in the original ... |
| `replace_key_prefix_with` | String | No | The object key prefix to use in the redirect request. For example, to redirect requests for all page... |
| `replace_key_with` | String | No | The specific object key to use in the redirect request. For example, redirect request to ``error.htm... |

### ReplicaModifications

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | [Enum (ReplicaModificationsStatus)](#status-replicamodificationsstatus) | Yes | Specifies whether Amazon S3 replicates modifications on replicas. *Allowed values*: ``Enabled`` | ``... |

### ReplicationConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `role` | IamRoleArn | Yes | The Amazon Resource Name (ARN) of the IAMlong (IAM) role that Amazon S3 assumes when replicating obj... |
| `rules` | [List\<ReplicationRule\>](#replicationrule) | Yes | A container for one or more replication rules. A replication configuration must have at least one ru... |

### ReplicationDestination

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `access_control_translation` | [Struct(AccessControlTranslation)](#accesscontroltranslation) | No | Specify this only in a cross-account scenario (where source and destination bucket owners are not th... |
| `account` | AwsAccountId | No | Destination bucket owner account ID. In a cross-account scenario, if you direct Amazon S3 to change ... |
| `bucket` | String | Yes | The Amazon Resource Name (ARN) of the bucket where you want Amazon S3 to store the results. |
| `encryption_configuration` | [Struct(EncryptionConfiguration)](#encryptionconfiguration) | No | Specifies encryption-related information. |
| `metrics` | [Struct(Metrics)](#metrics) | No | A container specifying replication metrics-related settings enabling replication metrics and events. |
| `replication_time` | [Struct(ReplicationTime)](#replicationtime) | No | A container specifying S3 Replication Time Control (S3 RTC), including whether S3 RTC is enabled and... |
| `storage_class` | [Enum (ReplicationDestinationStorageClass)](#storage_class-replicationdestinationstorageclass) | No | The storage class to use when replicating objects, such as S3 Standard or reduced redundancy. By def... |

### ReplicationRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `delete_marker_replication` | [Struct(DeleteMarkerReplication)](#deletemarkerreplication) | No | Specifies whether Amazon S3 replicates delete markers. If you specify a ``Filter`` in your replicati... |
| `destination` | [Struct(ReplicationDestination)](#replicationdestination) | Yes | A container for information about the replication destination and its configurations including enabl... |
| `filter` | [Struct(ReplicationRuleFilter)](#replicationrulefilter) | No | A filter that identifies the subset of objects to which the replication rule applies. A ``Filter`` m... |
| `id` | String(len: ..=255) | No | A unique identifier for the rule. The maximum value is 255 characters. If you don't specify a value,... |
| `prefix` | String(len: ..=1024) | No | An object key name prefix that identifies the object or objects to which the rule applies. The maxim... |
| `priority` | Int | No | The priority indicates which rule has precedence whenever two or more replication rules conflict. Am... |
| `source_selection_criteria` | [Struct(SourceSelectionCriteria)](#sourceselectioncriteria) | No | A container that describes additional filters for identifying the source objects that you want to re... |
| `status` | [Enum (ReplicationRuleStatus)](#status-replicationrulestatus) | Yes | Specifies whether the rule is enabled. |

### ReplicationRuleAndOperator

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `prefix` | String | No | An object key name prefix that identifies the subset of objects to which the rule applies. |
| `tag_filters` | `List<Map>` | No | An array of tags containing key and value pairs. |

### ReplicationRuleFilter

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `and` | [Struct(ReplicationRuleAndOperator)](#replicationruleandoperator) | No | A container for specifying rule filters. The filters determine the subset of objects to which the ru... |
| `prefix` | String | No | An object key name prefix that identifies the subset of objects to which the rule applies. Replaceme... |
| `tag_filter` | Map | No | A container for specifying a tag key and value. The rule applies only to objects that have the tag i... |

### ReplicationTime

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | [Enum (ReplicationTimeStatus)](#status-replicationtimestatus) | Yes | Specifies whether the replication time is enabled. |
| `time` | [Struct(ReplicationTimeValue)](#replicationtimevalue) | Yes | A container specifying the time by which replication should be complete for all objects and operatio... |

### ReplicationTimeValue

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `minutes` | Int | Yes | Contains an integer specifying time in minutes. Valid value: 15 |

### RoutingRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `redirect_rule` | [Struct(RedirectRule)](#redirectrule) | Yes | Container for redirect information. You can redirect requests to another host, to another page, or w... |
| `routing_rule_condition` | [Struct(RoutingRuleCondition)](#routingrulecondition) | No | A container for describing a condition that must be met for the specified redirect to apply. For exa... |

### RoutingRuleCondition

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `http_error_code_returned_equals` | String | No | The HTTP error code when the redirect is applied. In the event of an error, if the error code equals... |
| `key_prefix_equals` | String | No | The object key name prefix when the redirect is applied. For example, to redirect requests for ``Exa... |

### Rule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `abort_incomplete_multipart_upload` | [Struct(AbortIncompleteMultipartUpload)](#abortincompletemultipartupload) | No | Specifies a lifecycle rule that stops incomplete multipart uploads to an Amazon S3 bucket. |
| `expiration_date` | String | No | Indicates when objects are deleted from Amazon S3 and Amazon S3 Glacier. The date value must be in I... |
| `expiration_in_days` | Int | No | Indicates the number of days after creation when objects are deleted from Amazon S3 and Amazon S3 Gl... |
| `expired_object_delete_marker` | Bool | No | Indicates whether Amazon S3 will remove a delete marker without any noncurrent versions. If set to t... |
| `id` | String(len: ..=255) | No | Unique identifier for the rule. The value can't be longer than 255 characters. |
| `noncurrent_version_expiration` | [Struct(NoncurrentVersionExpiration)](#noncurrentversionexpiration) | No | Specifies when noncurrent object versions expire. Upon expiration, S3 permanently deletes the noncur... |
| `noncurrent_version_expiration_in_days` | Int | No | (Deprecated.) For buckets with versioning enabled (or suspended), specifies the time, in days, betwe... |
| `noncurrent_version_transition` | [Struct(NoncurrentVersionTransition)](#noncurrentversiontransition) | No | (Deprecated.) For buckets with versioning enabled (or suspended), specifies when non-current objects... |
| `noncurrent_version_transitions` | [List\<NoncurrentVersionTransition\>](#noncurrentversiontransition) | No | For buckets with versioning enabled (or suspended), one or more transition rules that specify when n... |
| `object_size_greater_than` | NumericString(len: ..=20) | No | Specifies the minimum object size in bytes for this rule to apply to. Objects must be larger than th... |
| `object_size_less_than` | NumericString(len: ..=20) | No | Specifies the maximum object size in bytes for this rule to apply to. Objects must be smaller than t... |
| `prefix` | String | No | Object key prefix that identifies one or more objects to which this rule applies. Replacement must b... |
| `status` | [Enum (RuleStatus)](#status-rulestatus) | Yes | If ``Enabled``, the rule is currently being applied. If ``Disabled``, the rule is not currently bein... |
| `tag_filters` | `List<Map>` | No | Tags to use to identify a subset of objects to which the lifecycle rule applies. |
| `transition` | [Struct(Transition)](#transition) | No | (Deprecated.) Specifies when an object transitions to a specified storage class. If you specify an e... |
| `transitions` | [List\<Transition\>](#transition) | No | One or more transition rules that specify when an object transitions to a specified storage class. I... |

### S3KeyFilter

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rules` | [List\<FilterRule\>](#filterrule) | Yes | A list of containers for the key-value pair that defines the criteria for the filter rule. |

### S3TablesDestination

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `table_arn` | Arn | No | The Amazon Resource Name (ARN) for the metadata table in the metadata table configuration. The speci... |
| `table_bucket_arn` | Arn | Yes | The Amazon Resource Name (ARN) for the table bucket that's specified as the destination in the metad... |
| `table_name` | String | Yes | The name for the metadata table in your metadata table configuration. The specified metadata table n... |
| `table_namespace` | String | No | The table bucket namespace for the metadata table in your metadata table configuration. This value i... |

### ServerSideEncryptionByDefault

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kms_master_key_id` | KmsKeyId | No | AWS Key Management Service (KMS) customer managed key ID to use for the default encryption. + *Gener... |
| `sse_algorithm` | [Enum (ServerSideEncryptionByDefaultSseAlgorithm)](#sse_algorithm-serversideencryptionbydefaultssealgorithm) | Yes | Server-side encryption algorithm to use for the default encryption. For directory buckets, there are... |

### ServerSideEncryptionRule

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `blocked_encryption_types` | [Struct(BlockedEncryptionTypes)](#blockedencryptiontypes) | No | A bucket-level setting for Amazon S3 general purpose buckets used to prevent the upload of new objec... |
| `bucket_key_enabled` | Bool | No | Specifies whether Amazon S3 should use an S3 Bucket Key with server-side encryption using KMS (SSE-K... |
| `server_side_encryption_by_default` | [Struct(ServerSideEncryptionByDefault)](#serversideencryptionbydefault) | No | Specifies the default server-side encryption to apply to new objects in the bucket. If a PUT Object ... |

### SourceSelectionCriteria

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `replica_modifications` | [Struct(ReplicaModifications)](#replicamodifications) | No | A filter that you can specify for selection for modifications on replicas. |
| `sse_kms_encrypted_objects` | [Struct(SseKmsEncryptedObjects)](#ssekmsencryptedobjects) | No | A container for filter information for the selection of Amazon S3 objects encrypted with AWS KMS. |

### SseKmsEncryptedObjects

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | [Enum (SseKmsEncryptedObjectsStatus)](#status-ssekmsencryptedobjectsstatus) | Yes | Specifies whether Amazon S3 replicates objects created with server-side encryption using an AWS KMS ... |

### StorageClassAnalysis

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `data_export` | [Struct(DataExport)](#dataexport) | No | Specifies how data related to the storage class analysis for an Amazon S3 bucket should be exported. |

### TargetObjectKeyFormat

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `partitioned_prefix` | [Struct(PartitionedPrefix)](#partitionedprefix) | No |  |
| `simple_prefix` | Map | No | This format defaults the prefix to the given log file prefix for delivering server access log file. |

### Tiering

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `access_tier` | [Enum (AccessTier)](#access_tier-accesstier) | Yes | S3 Intelligent-Tiering access tier. See [Storage class for automatically optimizing frequently and i... |
| `days` | Int | Yes | The number of consecutive days of no access after which an object will be eligible to be transitione... |

### TopicConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `event` | String | Yes | The Amazon S3 bucket event about which to send notifications. For more information, see [Supported E... |
| `filter` | [Struct(NotificationFilter)](#notificationfilter) | No | The filtering rules that determine for which objects to send notifications. For example, you can cre... |
| `topic` | Arn | Yes | The Amazon Resource Name (ARN) of the Amazon SNS topic to which Amazon S3 publishes a message when i... |

### Transition

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `storage_class` | [Enum (TransitionStorageClass)](#storage_class-transitionstorageclass) | Yes | The storage class to which you want the object to transition. |
| `transition_date` | String | No | Indicates when objects are transitioned to the specified storage class. The date value must be in IS... |
| `transition_in_days` | Int | No | Indicates the number of days after creation when objects are transitioned to the specified storage c... |

### VersioningConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | [Enum (VersioningConfigurationStatus)](#status-versioningconfigurationstatus) | Yes | The versioning state of the bucket. |

### WebsiteConfiguration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `error_document` | String | No | The name of the error document for the website. |
| `index_document` | String | No | The name of the index document for the website. |
| `redirect_all_requests_to` | [Struct(RedirectAllRequestsTo)](#redirectallrequeststo) | No | The redirect behavior for every request to this bucket's website endpoint. If you specify this prope... |
| `routing_rules` | [List\<RoutingRule\>](#routingrule) | No | Rules that define when a redirect is applied and the redirect behavior. |

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

- **Type:** String(uri)


