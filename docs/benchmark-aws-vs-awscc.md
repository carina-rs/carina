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

## Key Findings

1. **Create operations**: AWS SDK is **8-14x faster** than Cloud Control. Cloud Control's abstraction layer adds significant overhead (10-25s vs 1-2s).

2. **Read operations**: AWS SDK is **1.5-2.4x faster**. The gap is smaller because both ultimately call the same AWS APIs, but Cloud Control adds serialization overhead.

3. **Destroy operations**: AWS SDK is **6-10x faster**. Cloud Control waits for resource stabilization, adding polling delays.

4. **Security Group** (with VPC dependency): Shows the largest gap because Cloud Control serializes the two resource operations, while the SDK provider can process them more efficiently.

## Implications

- For latency-sensitive workflows (development iteration, CI/CD), the `aws` provider provides significantly better UX
- For resources where the native SDK implementation doesn't exist, `awscc` provides functional coverage at the cost of speed
- Read operations (used during `plan`) have the smallest gap, so plan times are less affected than apply/destroy times

## Reproducing

```bash
aws-vault exec <profile> -- ./scripts/benchmark-providers.sh [iterations]
```
