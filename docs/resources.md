# Carina Resource Reference

This document describes all supported resources and their attributes.

## Table of Contents

- [S3 Resources](#s3-resources)
  - [aws.s3.bucket](#awss3bucket)
- [VPC Resources](#vpc-resources)
  - [aws.vpc](#awsvpc)
  - [aws.subnet](#awssubnet)
  - [aws.internet_gateway](#awsinternet_gateway)
  - [aws.route_table](#awsroute_table)
  - [aws.route](#awsroute)
  - [aws.security_group](#awssecurity_group)
  - [aws.security_group.ingress_rule](#awssecurity_groupingress_rule)
  - [aws.security_group.egress_rule](#awssecurity_groupegress_rule)

---

## S3 Resources

### aws.s3.bucket

An S3 bucket for object storage.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | String | No | Override bucket name (defaults to resource name) |
| `region` | aws.Region | **Yes** | The AWS region for the bucket |
| `acl` | Enum | No | The canned ACL for the bucket |
| `versioning` | Bool | No | Enable versioning for the bucket |
| `expiration_days` | Int | No | Number of days before objects expire |

#### ACL Values

- `private`
- `public_read`
- `public_read_write`
- `authenticated_read`

#### Example

```crn
provider aws {
    region = aws.Region.ap_northeast_1
}

aws.s3.bucket {
    name            = "my-application-bucket"
    region          = aws.Region.ap_northeast_1
    versioning      = true
    expiration_days = 90
}
```

---

## VPC Resources

### aws.vpc

An AWS VPC (Virtual Private Cloud).

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | VPC ID (read-only, set after creation) |
| `name` | String | **Yes** | VPC name (Name tag) |
| `region` | aws.Region | **Yes** | The AWS region for the VPC |
| `cidr_block` | CidrBlock | **Yes** | The IPv4 CIDR block for the VPC (e.g., "10.0.0.0/16") |
| `enable_dns_support` | Bool | No | Enable DNS resolution support (default: true) |
| `enable_dns_hostnames` | Bool | No | Enable DNS hostnames |

#### Example

```crn
let main_vpc = aws.vpc {
    name                 = "main-vpc"
    region               = aws.Region.ap_northeast_1
    cidr_block           = "10.0.0.0/16"
    enable_dns_support   = true
    enable_dns_hostnames = true
}
```

#### Notes

- `cidr_block` is immutable after creation
- `id` is the VPC ID assigned by AWS after creation (e.g., "vpc-12345678")

---

### aws.subnet

An AWS VPC Subnet.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | Subnet ID (read-only, set after creation) |
| `name` | String | **Yes** | Subnet name (Name tag) |
| `region` | aws.Region | **Yes** | The AWS region for the subnet |
| `vpc_id` | String | **Yes** | VPC ID to create the subnet in |
| `cidr_block` | CidrBlock | **Yes** | The IPv4 CIDR block for the subnet |
| `availability_zone` | aws.AvailabilityZone | No | The availability zone (e.g., aws.AvailabilityZone.ap_northeast_1a) |

#### Example

```crn
let public_subnet_1a = aws.subnet {
    name              = "public-subnet-1a"
    region            = aws.Region.ap_northeast_1
    vpc_id            = main_vpc.id
    cidr_block        = "10.0.1.0/24"
    availability_zone = aws.AvailabilityZone.ap_northeast_1a
}

let public_subnet_1c = aws.subnet {
    name              = "public-subnet-1c"
    region            = aws.Region.ap_northeast_1
    vpc_id            = main_vpc.id
    cidr_block        = "10.0.2.0/24"
    availability_zone = aws.AvailabilityZone.ap_northeast_1c
}
```

#### Notes

- `cidr_block`, `vpc_id`, and `availability_zone` are immutable after creation

---

### aws.internet_gateway

An AWS Internet Gateway for VPC internet access.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | Internet Gateway ID (read-only, set after creation) |
| `name` | String | **Yes** | Internet Gateway name (Name tag) |
| `region` | aws.Region | **Yes** | The AWS region for the Internet Gateway |
| `vpc_id` | String | No | VPC ID to attach the Internet Gateway to |

#### Example

```crn
let igw = aws.internet_gateway {
    name   = "main-igw"
    region = aws.Region.ap_northeast_1
    vpc_id = main_vpc.id
}
```

---

### aws.route_table

An AWS VPC Route Table.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | Route Table ID (read-only, set after creation) |
| `name` | String | **Yes** | Route Table name (Name tag) |
| `region` | aws.Region | **Yes** | The AWS region for the Route Table |
| `vpc_id` | String | **Yes** | VPC ID for the Route Table |

#### Example

```crn
let public_rt = aws.route_table {
    name   = "public-rt"
    region = aws.Region.ap_northeast_1
    vpc_id = main_vpc.id
}
```

#### Notes

- Use `aws.route` to add routes to the route table

---

### aws.route

A route in an AWS VPC Route Table.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | String | **Yes** | Route name (for identification) |
| `region` | aws.Region | **Yes** | The AWS region |
| `route_table_id` | String | **Yes** | Route Table ID to add the route to |
| `destination_cidr_block` | CidrBlock | **Yes** | Destination CIDR block (e.g., "0.0.0.0/0") |
| `gateway_id` | String | No | Internet Gateway ID (for internet-bound traffic) |
| `nat_gateway_id` | String | No | NAT Gateway ID |

#### Example

```crn
aws.route {
    name                   = "public-route"
    region                 = aws.Region.ap_northeast_1
    route_table_id         = public_rt.id
    destination_cidr_block = "0.0.0.0/0"
    gateway_id             = igw.id
}
```

---

### aws.security_group

An AWS VPC Security Group.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | Security Group ID (read-only, set after creation) |
| `name` | String | **Yes** | Security Group name (Name tag) |
| `region` | aws.Region | **Yes** | The AWS region for the Security Group |
| `vpc_id` | String | **Yes** | VPC ID for the Security Group |
| `description` | String | No | Description of the Security Group |

#### Example

```crn
let web_sg = aws.security_group {
    name        = "web-sg"
    region      = aws.Region.ap_northeast_1
    vpc_id      = main_vpc.id
    description = "Web server security group"
}
```

#### Notes

- Use `aws.security_group.ingress_rule` and `aws.security_group.egress_rule` to define rules

---

### aws.security_group.ingress_rule

An inbound rule for an AWS VPC Security Group.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | Security Group Rule ID (read-only, set after creation) |
| `name` | String | **Yes** | Rule name (for identification) |
| `region` | aws.Region | **Yes** | The AWS region |
| `security_group_id` | String | **Yes** | Security Group ID to add the rule to |
| `protocol` | aws.Protocol | **Yes** | Protocol (aws.Protocol.tcp, aws.Protocol.udp, aws.Protocol.icmp, aws.Protocol.all) |
| `from_port` | Int | **Yes** | Start of port range (0-65535) |
| `to_port` | Int | **Yes** | End of port range (0-65535) |
| `cidr` | CidrBlock | No | CIDR block to allow (e.g., "0.0.0.0/0") |

#### Example

```crn
aws.security_group.ingress_rule {
    name              = "web-sg-http"
    region            = aws.Region.ap_northeast_1
    security_group_id = web_sg.id
    protocol          = aws.Protocol.tcp
    from_port         = 80
    to_port           = 80
    cidr              = "0.0.0.0/0"
}

aws.security_group.ingress_rule {
    name              = "web-sg-https"
    region            = aws.Region.ap_northeast_1
    security_group_id = web_sg.id
    protocol          = aws.Protocol.tcp
    from_port         = 443
    to_port           = 443
    cidr              = "0.0.0.0/0"
}
```

---

### aws.security_group.egress_rule

An outbound rule for an AWS VPC Security Group.

#### Attributes

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | String | No | Security Group Rule ID (read-only, set after creation) |
| `name` | String | **Yes** | Rule name (for identification) |
| `region` | aws.Region | **Yes** | The AWS region |
| `security_group_id` | String | **Yes** | Security Group ID to add the rule to |
| `protocol` | aws.Protocol | **Yes** | Protocol (aws.Protocol.tcp, aws.Protocol.udp, aws.Protocol.icmp, aws.Protocol.all) |
| `from_port` | Int | **Yes** | Start of port range (0-65535) |
| `to_port` | Int | **Yes** | End of port range (0-65535) |
| `cidr` | CidrBlock | No | CIDR block to allow (e.g., "0.0.0.0/0") |

#### Example

```crn
aws.security_group.egress_rule {
    name              = "web-sg-all-outbound"
    region            = aws.Region.ap_northeast_1
    security_group_id = web_sg.id
    protocol          = aws.Protocol.all
    from_port         = 0
    to_port           = 0
    cidr              = "0.0.0.0/0"
}
```

---

## AWS Protocols

The `aws.Protocol` type supports the following values for security group rules:

| DSL Value | AWS Value | Description |
|-----------|-----------|-------------|
| `aws.Protocol.tcp` | tcp | Transmission Control Protocol |
| `aws.Protocol.udp` | udp | User Datagram Protocol |
| `aws.Protocol.icmp` | icmp | Internet Control Message Protocol |
| `aws.Protocol.all` | -1 | All protocols |

---

## AWS Regions

The `aws.Region` type supports the following values:

| DSL Value | AWS Region |
|-----------|------------|
| `aws.Region.ap_northeast_1` | ap-northeast-1 (Tokyo) |
| `aws.Region.ap_northeast_2` | ap-northeast-2 (Seoul) |
| `aws.Region.ap_northeast_3` | ap-northeast-3 (Osaka) |
| `aws.Region.ap_southeast_1` | ap-southeast-1 (Singapore) |
| `aws.Region.ap_southeast_2` | ap-southeast-2 (Sydney) |
| `aws.Region.ap_south_1` | ap-south-1 (Mumbai) |
| `aws.Region.us_east_1` | us-east-1 (N. Virginia) |
| `aws.Region.us_east_2` | us-east-2 (Ohio) |
| `aws.Region.us_west_1` | us-west-1 (N. California) |
| `aws.Region.us_west_2` | us-west-2 (Oregon) |
| `aws.Region.eu_west_1` | eu-west-1 (Ireland) |
| `aws.Region.eu_west_2` | eu-west-2 (London) |
| `aws.Region.eu_central_1` | eu-central-1 (Frankfurt) |

---

## Complete Example

```crn
provider aws {
    region = aws.Region.ap_northeast_1
}

// VPC
let main_vpc = aws.vpc {
    name                 = "production-vpc"
    region               = aws.Region.ap_northeast_1
    cidr_block           = "10.0.0.0/16"
    enable_dns_support   = true
    enable_dns_hostnames = true
}

// Subnets
let public_subnet_1a = aws.subnet {
    name              = "public-subnet-1a"
    region            = aws.Region.ap_northeast_1
    vpc_id            = main_vpc.id
    cidr_block        = "10.0.1.0/24"
    availability_zone = aws.AvailabilityZone.ap_northeast_1a
}

let public_subnet_1c = aws.subnet {
    name              = "public-subnet-1c"
    region            = aws.Region.ap_northeast_1
    vpc_id            = main_vpc.id
    cidr_block        = "10.0.2.0/24"
    availability_zone = aws.AvailabilityZone.ap_northeast_1c
}

let private_subnet_1a = aws.subnet {
    name              = "private-subnet-1a"
    region            = aws.Region.ap_northeast_1
    vpc_id            = main_vpc.id
    cidr_block        = "10.0.10.0/24"
    availability_zone = aws.AvailabilityZone.ap_northeast_1a
}

// Internet Gateway
let igw = aws.internet_gateway {
    name   = "production-igw"
    region = aws.Region.ap_northeast_1
    vpc_id = main_vpc.id
}

// Route Table for public subnets
let public_rt = aws.route_table {
    name   = "public-rt"
    region = aws.Region.ap_northeast_1
    vpc_id = main_vpc.id
}

// Route for internet access
aws.route {
    name                   = "public-route"
    region                 = aws.Region.ap_northeast_1
    route_table_id         = public_rt.id
    destination_cidr_block = "0.0.0.0/0"
    gateway_id             = igw.id
}

// Security Groups
let web_sg = aws.security_group {
    name        = "web-sg"
    region      = aws.Region.ap_northeast_1
    vpc_id      = main_vpc.id
    description = "Web server security group"
}

// Web Security Group Rules
aws.security_group.ingress_rule {
    name              = "web-sg-http"
    region            = aws.Region.ap_northeast_1
    security_group_id = web_sg.id
    protocol          = aws.Protocol.tcp
    from_port         = 80
    to_port           = 80
    cidr              = "0.0.0.0/0"
}

aws.security_group.ingress_rule {
    name              = "web-sg-https"
    region            = aws.Region.ap_northeast_1
    security_group_id = web_sg.id
    protocol          = aws.Protocol.tcp
    from_port         = 443
    to_port           = 443
    cidr              = "0.0.0.0/0"
}

aws.security_group.egress_rule {
    name              = "web-sg-all-outbound"
    region            = aws.Region.ap_northeast_1
    security_group_id = web_sg.id
    protocol          = aws.Protocol.all
    from_port         = 0
    to_port           = 0
    cidr              = "0.0.0.0/0"
}

// Database Security Group
let db_sg = aws.security_group {
    name        = "db-sg"
    region      = aws.Region.ap_northeast_1
    vpc_id      = main_vpc.id
    description = "Database security group"
}

aws.security_group.ingress_rule {
    name              = "db-sg-mysql"
    region            = aws.Region.ap_northeast_1
    security_group_id = db_sg.id
    protocol          = aws.Protocol.tcp
    from_port         = 3306
    to_port           = 3306
    cidr              = "10.0.0.0/16"
}

aws.security_group.egress_rule {
    name              = "db-sg-all-outbound"
    region            = aws.Region.ap_northeast_1
    security_group_id = db_sg.id
    protocol          = aws.Protocol.all
    from_port         = 0
    to_port           = 0
    cidr              = "0.0.0.0/0"
}

// S3 Bucket
aws.s3.bucket {
    name       = "production-assets-bucket"
    region     = aws.Region.ap_northeast_1
    versioning = true
}
```
