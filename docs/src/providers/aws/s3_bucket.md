# aws.s3.bucket

CloudFormation Type: `AWS::S3::Bucket`

## Argument Reference

### `object_ownership`

- **Type:** [Enum (ObjectOwnership)](#object_ownership-objectownership)
- **Required:** No

### `versioning_status`

- **Type:** [Enum (VersioningStatus)](#versioning_status-versioningstatus)
- **Required:** No

The versioning state of the bucket.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Enum Values

### object_ownership (ObjectOwnership)

| Value | DSL Identifier |
|-------|----------------|
| `BucketOwnerEnforced` | `aws.s3.bucket.ObjectOwnership.BucketOwnerEnforced` |
| `BucketOwnerPreferred` | `aws.s3.bucket.ObjectOwnership.BucketOwnerPreferred` |
| `ObjectWriter` | `aws.s3.bucket.ObjectOwnership.ObjectWriter` |

Shorthand formats: `BucketOwnerEnforced` or `ObjectOwnership.BucketOwnerEnforced`

### versioning_status (VersioningStatus)

| Value | DSL Identifier |
|-------|----------------|
| `Enabled` | `aws.s3.bucket.VersioningStatus.Enabled` |
| `Suspended` | `aws.s3.bucket.VersioningStatus.Suspended` |

Shorthand formats: `Enabled` or `VersioningStatus.Enabled`

