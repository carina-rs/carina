#!/bin/bash
# Benchmark: vpc_full (multi-resource) comparison between aws and awscc
#
# Usage:
#   aws-vault exec carina-test-000 -- ./scripts/bench-vpc-full.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

CARINA="./target/release/carina"

echo "Building carina (release)..."
cargo build --release --bin carina --quiet 2>/dev/null || cargo build --release --bin carina

TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# AWS version of vpc_full (simplified: VPC + IGW + attachment + 3 subnets + route table + 3 associations + EIP + NAT GW)
cat > "$TMPDIR/aws_vpc_full.crn" << 'AWSEOF'
provider aws {
  region = aws.Region.ap_northeast_1
}

let vpc = aws.ec2.vpc {
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags = { Name = "bench-aws" }
}

let igw = aws.ec2.internet_gateway {
  vpc_id = vpc.vpc_id
  tags = { Name = "bench-aws" }
}

let public_subnet_1a = aws.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.0.0/24"
  availability_zone = aws.AvailabilityZone.ap_northeast_1a
  tags = { Name = "public-1a" }
}

let public_subnet_1c = aws.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = aws.AvailabilityZone.ap_northeast_1c
  tags = { Name = "public-1c" }
}

let public_rt = aws.ec2.route_table {
  vpc_id = vpc.vpc_id
  tags = { Name = "public" }
}

aws.ec2.route {
  route_table_id         = public_rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw.internet_gateway_id
}

aws.ec2.subnet_route_table_association {
  subnet_id      = public_subnet_1a.subnet_id
  route_table_id = public_rt.route_table_id
}

aws.ec2.subnet_route_table_association {
  subnet_id      = public_subnet_1c.subnet_id
  route_table_id = public_rt.route_table_id
}

let eip = aws.ec2.eip {
  domain = aws.ec2.eip.Domain.vpc
  tags = { Name = "nat-gw" }
}

let nat_gw = aws.ec2.nat_gateway {
  allocation_id     = eip.allocation_id
  subnet_id         = public_subnet_1a.subnet_id
  connectivity_type = aws.ec2.nat_gateway.ConnectivityType.public
  tags = { Name = "nat-gw" }
}

let private_subnet_1a = aws.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.100.0/24"
  availability_zone = aws.AvailabilityZone.ap_northeast_1a
  tags = { Name = "private-1a" }
}

let private_rt = aws.ec2.route_table {
  vpc_id = vpc.vpc_id
  tags = { Name = "private" }
}

aws.ec2.route {
  route_table_id         = private_rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id         = nat_gw.nat_gateway_id
}

aws.ec2.subnet_route_table_association {
  subnet_id      = private_subnet_1a.subnet_id
  route_table_id = private_rt.route_table_id
}

let sg = aws.ec2.security_group {
  group_name  = "bench-sg"
  description = "Benchmark SG"
  vpc_id      = vpc.vpc_id
  tags = { Name = "bench-sg" }
}

aws.ec2.security_group_ingress {
  group_id    = sg.group_id
  ip_protocol = "tcp"
  from_port   = 443
  to_port     = 443
  cidr_ip     = "10.0.0.0/16"
}
AWSEOF

# AWSCC version (same topology)
cat > "$TMPDIR/awscc_vpc_full.crn" << 'AWSCCEOF'
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags = { Name = "bench-awscc" }
}

let igw = awscc.ec2.internet_gateway {
  tags = { Name = "bench-awscc" }
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}

let public_subnet_1a = awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.0.0/24"
  availability_zone = "ap-northeast-1a"
  tags = { Name = "public-1a" }
}

let public_subnet_1c = awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = "ap-northeast-1c"
  tags = { Name = "public-1c" }
}

let public_rt = awscc.ec2.route_table {
  vpc_id = vpc.vpc_id
  tags = { Name = "public" }
}

awscc.ec2.route {
  route_table_id         = public_rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw_attachment.internet_gateway_id
}

awscc.ec2.subnet_route_table_association {
  subnet_id      = public_subnet_1a.subnet_id
  route_table_id = public_rt.route_table_id
}

awscc.ec2.subnet_route_table_association {
  subnet_id      = public_subnet_1c.subnet_id
  route_table_id = public_rt.route_table_id
}

let eip = awscc.ec2.eip {
  domain = "vpc"
  tags = { Name = "nat-gw" }
}

let nat_gw = awscc.ec2.nat_gateway {
  allocation_id = eip.allocation_id
  subnet_id     = public_subnet_1a.subnet_id
  tags = { Name = "nat-gw" }
}

let private_subnet_1a = awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = "10.0.100.0/24"
  availability_zone = "ap-northeast-1a"
  tags = { Name = "private-1a" }
}

let private_rt = awscc.ec2.route_table {
  vpc_id = vpc.vpc_id
  tags = { Name = "private" }
}

awscc.ec2.route {
  route_table_id         = private_rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  nat_gateway_id         = nat_gw.nat_gateway_id
}

awscc.ec2.subnet_route_table_association {
  subnet_id      = private_subnet_1a.subnet_id
  route_table_id = private_rt.route_table_id
}

let sg = awscc.ec2.security_group {
  vpc_id            = vpc.vpc_id
  group_description = "Benchmark SG"
  group_name        = "bench-sg"
  tags = { Name = "bench-sg" }
}

awscc.ec2.security_group_ingress {
  group_id    = sg.group_id
  description = "Allow HTTPS from VPC"
  ip_protocol = "tcp"
  from_port   = 443
  to_port     = 443
  cidr_ip     = "10.0.0.0/16"
}
AWSCCEOF

echo ""
echo "VPC Full Benchmark (16 resources)"
echo "════════════════════════════════════════"
echo ""

time_it() {
    local start end elapsed
    start=$(python3 -c 'import time; print(int(time.time()*1000))')
    "$@" > /dev/null 2>&1
    end=$(python3 -c 'import time; print(int(time.time()*1000))')
    elapsed=$((end - start))
    echo "$elapsed"
}

for provider in aws awscc; do
    echo "--- $provider provider ---"
    workdir="$TMPDIR/${provider}_vpc_full"
    mkdir -p "$workdir"
    cp "$TMPDIR/${provider}_vpc_full.crn" "$workdir/main.crn"

    echo -n "  Apply:   "
    t=$(time_it "$CARINA" apply "$workdir/main.crn" --auto-approve)
    echo "${t}ms ($(echo "scale=1; $t/1000" | bc)s)"

    echo -n "  Plan:    "
    t=$(time_it "$CARINA" plan "$workdir/main.crn")
    echo "${t}ms ($(echo "scale=1; $t/1000" | bc)s)"

    echo -n "  Destroy: "
    t=$(time_it "$CARINA" destroy "$workdir/main.crn" --auto-approve)
    echo "${t}ms ($(echo "scale=1; $t/1000" | bc)s)"

    echo ""
done
