# awscc.sts.caller_identity

Data source that returns the AWS account ID, ARN, and user ID of the caller via STS GetCallerIdentity.

## Usage

```crn
let identity = read awscc.sts.caller_identity {}
```

The returned attributes can be referenced by other resources:

```crn
let vpc_pool = awscc.ec2.ipam_pool {
  source_resource = [
    {
      resource_owner = identity.account_id
    }
  ]
}
```

## Attributes (read-only)

| Attribute | Type | Description |
|-----------|------|-------------|
| `account_id` | String | The AWS account ID of the caller |
| `arn` | String | The ARN of the IAM principal making the call |
| `user_id` | String | The unique identifier of the calling entity |
