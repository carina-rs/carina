# AWSCC Provider

The `awscc` provider manages AWS resources through the [AWS Cloud Control API](https://docs.aws.amazon.com/cloudcontrolapi/latest/userguide/what-is-cloudcontrolapi.html).

## Configuration

```crn
provider awscc {
  region = aws.Region.ap_northeast_1
}
```

## Usage

Resources are defined using the `awscc.<resource_type>` syntax:

```crn
let vpc = awscc.ec2_vpc {
  name       = "my-vpc"
  cidr_block = "10.0.0.0/16"
  tags = {
    Environment = "production"
  }
}
```

Named resources (using `let`) can be referenced by other resources:

```crn
let subnet = awscc.ec2_subnet {
  name              = "my-subnet"
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = "ap-northeast-1a"
}
```

## Enum Values

Some attributes accept enum values. These can be specified in three formats:

- **Bare value**: `instance_tenancy = default`
- **TypeName.value**: `instance_tenancy = InstanceTenancy.default`
- **Full namespace**: `instance_tenancy = awscc.ec2_vpc.InstanceTenancy.default`
