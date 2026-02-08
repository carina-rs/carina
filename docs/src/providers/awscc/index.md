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

## Supported Resource Types

| Resource Type | CloudFormation Type | Description |
|---|---|---|
| [ec2_vpc](ec2_vpc.md) | AWS::EC2::VPC | Virtual Private Cloud |
| [ec2_subnet](ec2_subnet.md) | AWS::EC2::Subnet | VPC Subnet |
| [ec2_internet_gateway](ec2_internet_gateway.md) | AWS::EC2::InternetGateway | Internet Gateway |
| [ec2_vpc_gateway_attachment](ec2_vpc_gateway_attachment.md) | AWS::EC2::VPCGatewayAttachment | VPC Gateway Attachment |
| [ec2_route_table](ec2_route_table.md) | AWS::EC2::RouteTable | Route Table |
| [ec2_route](ec2_route.md) | AWS::EC2::Route | Route |
| [ec2_subnet_route_table_association](ec2_subnet_route_table_association.md) | AWS::EC2::SubnetRouteTableAssociation | Subnet Route Table Association |
| [ec2_eip](ec2_eip.md) | AWS::EC2::EIP | Elastic IP Address |
| [ec2_nat_gateway](ec2_nat_gateway.md) | AWS::EC2::NatGateway | NAT Gateway |
| [ec2_security_group](ec2_security_group.md) | AWS::EC2::SecurityGroup | Security Group |
| [ec2_security_group_ingress](ec2_security_group_ingress.md) | AWS::EC2::SecurityGroupIngress | Security Group Ingress Rule |
| [ec2_security_group_egress](ec2_security_group_egress.md) | AWS::EC2::SecurityGroupEgress | Security Group Egress Rule |
| [ec2_vpc_endpoint](ec2_vpc_endpoint.md) | AWS::EC2::VPCEndpoint | VPC Endpoint |
| [ec2_flow_log](ec2_flow_log.md) | AWS::EC2::FlowLog | VPC Flow Log |
