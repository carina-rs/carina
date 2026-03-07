# aws.s3.bucket

CloudFormation Type: `AWS::S3::Bucket`

## Argument Reference

### `acl`

- **Type:** [Enum (ACL)](#acl-acl)
- **Required:** No

The canned ACL to apply to the bucket. This functionality is not supported for directory buckets.

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket to create. General purpose buckets - For information about bucket naming restrictions, see Bucket naming rules in the Amazon S3 User Guide. Directory buckets - When you use this operation with a directory bucket, you must use path-style requests in the format https://s3express-control.region-code.amazonaws.com/bucket-name . Virtual-hosted-style requests aren't supported. Directory bucket names must be unique in the chosen Zone (Availability Zone or Local Zone). Bucket names must also follow the format bucket-base-name--zone-id--x-s3 (for example, DOC-EXAMPLE-BUCKET--usw2-az1--x-s3). For information about bucket naming restrictions, see Directory bucket naming rules in the Amazon S3 User Guide

### `grant_full_control`

- **Type:** String
- **Required:** No

Allows grantee the read, write, read ACP, and write ACP permissions on the bucket. This functionality is not supported for directory buckets.

### `grant_read`

- **Type:** String
- **Required:** No

Allows grantee to list the objects in the bucket. This functionality is not supported for directory buckets.

### `grant_read_acp`

- **Type:** String
- **Required:** No

Allows grantee to read the bucket ACL. This functionality is not supported for directory buckets.

### `grant_write`

- **Type:** String
- **Required:** No

Allows grantee to create new objects in the bucket. For the bucket and object owners of existing objects, also allows deletions and overwrites of those objects. This functionality is not supported for directory buckets.

### `grant_write_acp`

- **Type:** String
- **Required:** No

Allows grantee to write the ACL for the applicable bucket. This functionality is not supported for directory buckets.

### `object_lock_enabled_for_bucket`

- **Type:** Bool
- **Required:** No

Specifies whether you want S3 Object Lock to be enabled for the new bucket. This functionality is not supported for directory buckets.

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

### acl (ACL)

| Value | DSL Identifier |
|-------|----------------|
| `authenticated-read` | `aws.s3.bucket.ACL.authenticated_read` |
| `private` | `aws.s3.bucket.ACL.private` |
| `public-read` | `aws.s3.bucket.ACL.public_read` |
| `public-read-write` | `aws.s3.bucket.ACL.public_read_write` |

Shorthand formats: `authenticated_read` or `ACL.authenticated_read`

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

