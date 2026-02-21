# AWS Provider

The `aws` provider manages AWS resources through native AWS SDK APIs (EC2, S3).

## Configuration

```crn
provider aws {
  region = aws.Region.ap_northeast_1
}
```

## Usage

Resources are defined using the `aws.<resource_type>` syntax:

```crn
let vpc = aws.ec2_vpc {
  name       = "my-vpc"
  cidr_block = "10.0.0.0/16"
  tags = {
    Environment = "production"
  }
}
```

Named resources (using `let`) can be referenced by other resources:

```crn
let subnet = aws.ec2_subnet {
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
- **Full namespace**: `instance_tenancy = aws.ec2_vpc.InstanceTenancy.default`
