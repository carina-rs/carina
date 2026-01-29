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

VPC example using the AWS Cloud Control provider.

- Using the `awscc` provider (AWS Cloud Control API)
- VPC creation with DNS settings

The awscc provider uses AWS Cloud Control API which provides a consistent interface for managing AWS resources.

```bash
cargo run --bin carina -- validate examples/awscc-vpc/
cargo run --bin carina -- plan examples/awscc-vpc/
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
