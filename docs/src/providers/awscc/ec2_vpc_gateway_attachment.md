# awscc.ec2.vpc_gateway_attachment

CloudFormation Type: `AWS::EC2::VPCGatewayAttachment`

Resource Type definition for AWS::EC2::VPCGatewayAttachment

## Argument Reference

### `internet_gateway_id`

- **Type:** InternetGatewayId
- **Required:** No

The ID of the internet gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.

### `vpc_id`

- **Type:** VpcId
- **Required:** Yes

The ID of the VPC.

### `vpn_gateway_id`

- **Type:** VpnGatewayId
- **Required:** No

The ID of the virtual private gateway. You must specify either InternetGatewayId or VpnGatewayId, but not both.

## Attribute Reference

### `attachment_type`

- **Type:** String



## Example

```crn
let vpc = awscc.ec2.vpc {
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}

let igw = awscc.ec2.internet_gateway {
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}
```
