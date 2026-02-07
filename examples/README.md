# Carina Examples

This directory contains example configurations demonstrating various Carina features.

## Examples

### aws-s3/

S3 bucket example with state backend configuration.

- S3 backend for state management
- Creating S3 buckets with versioning and lifecycle rules

```bash
cargo run --bin carina -- validate examples/aws-s3/
cargo run --bin carina -- plan examples/aws-s3/
```

### aws-vpc/

Comprehensive VPC example using the AWS provider.

- VPC with DNS settings
- Subnets in multiple availability zones
- Internet Gateway
- Route Table with routes
- Security Group with ingress/egress rules

```bash
cargo run --bin carina -- validate examples/aws-vpc/
cargo run --bin carina -- plan examples/aws-vpc/
```

### aws-module/

Module usage example demonstrating code reusability.

- Importing and using modules
- Resource references (let bindings)
- Passing parameters to modules
- Input/Output definitions

```bash
cargo run --bin carina -- validate examples/aws-module/
cargo run --bin carina -- plan examples/aws-module/
```

### awscc-vpc/

Comprehensive VPC example using the AWS Cloud Control provider.

- VPC with DNS support
- Public/Private/Database Subnets across 3 AZs
- Internet Gateway
- NAT Gateways (one per AZ)
- Route Tables with routes
- Security Group for VPC Endpoints
- VPC Endpoints (ECR, S3, CloudWatch Logs, SSM)

```bash
cargo run --bin carina -- validate examples/awscc-vpc/
cargo run --bin carina -- plan examples/awscc-vpc/
```

### awscc-vpc-module-example/

Module usage example with the AWS Cloud Control provider.

- Importing and using a VPC module
- Passing parameters to modules

```bash
cargo run --bin carina -- validate examples/awscc-vpc-module-example/
cargo run --bin carina -- plan examples/awscc-vpc-module-example/
```

## Running Examples

To validate an example:

```bash
cargo run --bin carina -- validate examples/<example-name>/
```

To see the execution plan:

```bash
aws-vault exec <profile> -- cargo run --bin carina -- plan examples/<example-name>/
```

To apply changes:

```bash
aws-vault exec <profile> -- cargo run --bin carina -- apply examples/<example-name>/
```
