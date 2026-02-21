# aws.s3.bucket

CloudFormation Type: `AWS::S3::Bucket`

## Argument Reference

### `versioning_status`

- **Type:** [Enum (VersioningStatus)](#versioning_status-versioningstatus)
- **Required:** No

The versioning state of the bucket.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### versioning_status (VersioningStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `aws.s3.bucket.VersioningStatus.Enabled` |
| `Suspended` | `aws.s3.bucket.VersioningStatus.Suspended` |

Shorthand formats: `Enabled` or `VersioningStatus.Enabled`

