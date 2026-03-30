# Provider Benchmark: aws vs awscc

Comparison of operation speed between the `aws` (native SDK) and `awscc` (Cloud Control API) providers for the same resources.

## Environment

- Region: ap-northeast-1
- Iterations: 3 per operation (averaged)
- Date: 2026-03-30
- Times include Carina overhead (parsing, diffing, state I/O)

## Results

| Resource             | Operation  |  aws (SDK) | awscc (CC) |  Ratio |
|----------------------|------------|------------|------------|--------|
| S3 Bucket            | Create     |       2.0s |      16.0s |   7.7x |
| S3 Bucket            | Read       |      376ms |      921ms |   2.4x |
| S3 Bucket            | Destroy    |      900ms |       6.1s |   6.8x |
| EC2 VPC              | Create     |       1.1s |      16.1s |  13.8x |
| EC2 VPC              | Read       |      382ms |      578ms |   1.5x |
| EC2 VPC              | Destroy    |      973ms |       5.7s |   5.9x |
| EC2 Security Group   | Create     |       1.9s |      26.5s |  13.4x |
| EC2 Security Group   | Read       |      434ms |      712ms |   1.6x |
| EC2 Security Group   | Destroy    |       1.1s |      10.8s |   9.5x |

## Multi-Resource Benchmark (VPC Full Stack)

Real-world infrastructure involves multiple dependent resources. This test deploys a VPC with subnets, route tables, NAT Gateway, security group, and associations (16 resources total).

| Operation          |  aws (SDK) | awscc (CC) | Ratio |
|--------------------|------------|------------|-------|
| Apply (16 resources)  |     130.2s |     152.5s |  1.2x |
| Plan (read all)       |       0.7s |       1.4s |  2.0x |
| Destroy (16 resources)|      57.9s |      69.3s |  1.2x |

The gap shrinks from 8-14x (single resource) to **~1.2x** in multi-resource scenarios. This is because Carina executes independent resources in parallel, so the bottleneck becomes the slowest resource in the dependency chain (NAT Gateway creation ~90s) rather than individual API call overhead.

## Key Findings

1. **Create operations**: AWS SDK is **8-14x faster** than Cloud Control. Cloud Control's abstraction layer adds significant overhead (10-25s vs 1-2s).

2. **Read operations**: AWS SDK is **1.5-2.4x faster**. The gap is smaller because both ultimately call the same AWS APIs, but Cloud Control adds serialization overhead.

3. **Destroy operations**: AWS SDK is **6-10x faster**. Cloud Control waits for resource stabilization, adding polling delays.

4. **Security Group** (with VPC dependency): Shows the largest gap because Cloud Control serializes the two resource operations, while the SDK provider can process them more efficiently.

## Implications

- **Single-resource operations**: The `aws` provider is dramatically faster (8-14x for create). This matters most for quick iteration on individual resources
- **Multi-resource deployments**: The gap narrows to ~1.2x because parallel execution masks individual API latency. Both providers are bottlenecked by slow resources (NAT Gateway, Transit Gateway)
- **Plan operations**: The `aws` provider is 1.5-2x faster for reads, which directly affects `carina plan` responsiveness
- **Overall**: The `aws` provider is consistently faster, but the practical difference in real-world multi-resource scenarios is modest (~20%)

## Reproducing

```bash
# Single-resource benchmarks (3 iterations each)
aws-vault exec <profile> -- ./scripts/benchmark-providers.sh [iterations]

# Multi-resource VPC full stack benchmark
aws-vault exec <profile> -- ./scripts/bench-vpc-full.sh
```
