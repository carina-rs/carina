# awscc.ec2_vpc_gateway_attachment

CloudFormation Type: `AWS::EC2::VPCGatewayAttachment`

Resource Type definition for AWS::EC2::VPCGatewayAttachment

## Argument Reference

### `internet_gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the internet gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.

### `vpc_id`

- **Type:** AwsResourceId
- **Required:** Yes

The ID of the VPC.

### `vpn_gateway_id`

- **Type:** AwsResourceId
- **Required:** No

The ID of the virtual private gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.

## Attribute Reference

### `attachment_type`

- **Type:** String



## Example

```crn
let vpc = awscc.ec2_vpc {
  name                 = "example-vpc"
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let igw = awscc.ec2_internet_gateway {
  name = "example-igw"
}

awscc.ec2_vpc_gateway_attachment {
  name                = "example-igw-attachment"
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}
```
