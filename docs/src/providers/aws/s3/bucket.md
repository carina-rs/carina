# aws.s3.bucket

CloudFormation Type: `AWS::S3::Bucket`

## Example

```crn
let bucket = aws.s3.bucket {
  bucket = "carina-example-s3-bucket"

  versioning_status = aws.s3.bucket.VersioningStatus.Enabled

  tags = {
    Environment = "example"
  }
}
```

## Argument Reference

### `acl`

- **Type:** [Enum (ACL)](#acl-acl)
- **Required:** No

The canned ACL to apply to the bucket. This functionality is not supported for directory buckets.

### `bucket`

- **Type:** String
- **Required:** Yes

The name of the bucket to create. General purpose buckets - For information about bucket naming restrictions, see Bucket naming rules in the Amazon S3 User Guide. Directory buckets - When you use this operation with a directory bucket, you must use path-style requests in the format https://s3express-control.region-code.amazonaws.com/bucket-name . Virtual-hosted-style requests aren't supported. Directory bucket names must be unique in the chosen Zone (Availability Zone or Local Zone). Bucket names must also follow the format bucket-base-name--zone-id--x-s3 (for example, DOC-EXAMPLE-BUCKET--usw2-az1--x-s3). For information about bucket naming restrictions, see Directory bucket naming rules in the Amazon S3 User Guide

### `bucket_namespace`

- **Type:** [Enum (BucketNamespace)](#bucket_namespace-bucketnamespace)
- **Required:** No

Specifies the namespace where you want to create your general purpose bucket. When you create a general purpose bucket, you can choose to create a bucket in the shared global namespace or you can choose to create a bucket in your account regional namespace. Your account regional namespace is a subdivision of the global namespace that only your account can create buckets in. For more information on bucket namespaces, see Namespaces for general purpose buckets. General purpose buckets in your account regional namespace must follow a specific naming convention. These buckets consist of a bucket name prefix that you create, and a suffix that contains your 12-digit Amazon Web Services Account ID, the Amazon Web Services Region code, and ends with -an. Bucket names must follow the format bucket-name-prefix-accountId-region-an (for example, amzn-s3-demo-bucket-111122223333-us-west-2-an). For information about bucket naming restrictions, see Account regional namespace naming rules in the Amazon S3 User Guide. This functionality is not supported for directory buckets.

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

### bucket_namespace (BucketNamespace)

| Value | DSL Identifier |
|-------|----------------|
| `account-regional` | `aws.s3.bucket.BucketNamespace.account_regional` |
| `global` | `aws.s3.bucket.BucketNamespace.global` |

Shorthand formats: `account_regional` or `BucketNamespace.account_regional`

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

