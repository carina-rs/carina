#!/bin/bash
# Benchmark: compare operation speed between aws and awscc providers
#
# Runs create/read/plan-verify/destroy for the same resource on both providers,
# measures wall-clock time, and outputs a comparison table.
#
# Usage:
#   aws-vault exec carina-test-000 -- ./scripts/benchmark-providers.sh [iterations]
#
# Default: 3 iterations per operation

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

ITERATIONS="${1:-3}"
CARINA="./target/release/carina"

echo "Building carina (release)..."
cargo build --release --bin carina --quiet 2>/dev/null || cargo build --release --bin carina

echo ""
echo "Provider Benchmark: aws vs awscc"
echo "Region: ap-northeast-1"
echo "Iterations: $ITERATIONS"
echo "════════════════════════════════════════"
echo ""

# Temp dir for test files and state
TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# ── Helper functions ────────────────────────────────────────────────

time_operation() {
    local label="$1"
    shift
    local start end elapsed
    start=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
    "$@" > /dev/null 2>&1
    local exit_code=$?
    end=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
    elapsed=$(( (end - start) / 1000000 ))  # ms
    echo "$elapsed"
    return $exit_code
}

run_benchmark() {
    local provider="$1"
    local resource_name="$2"
    local crn_file="$3"
    local workdir="$TMPDIR/${provider}_${resource_name}"
    mkdir -p "$workdir"
    cp "$crn_file" "$workdir/main.crn"

    local create_times=()
    local read_times=()
    local destroy_times=()

    for i in $(seq 1 "$ITERATIONS"); do
        # Clean state
        rm -f "$workdir/carina.state.json" "$workdir/carina.state.lock"

        # Create (apply)
        local t
        t=$(time_operation "create" "$CARINA" apply "$workdir/main.crn" --auto-approve)
        create_times+=("$t")

        # Read (plan with existing state = read + diff)
        t=$(time_operation "read" "$CARINA" plan "$workdir/main.crn")
        read_times+=("$t")

        # Destroy
        t=$(time_operation "destroy" "$CARINA" destroy "$workdir/main.crn" --auto-approve)
        destroy_times+=("$t")
    done

    # Calculate averages
    local create_avg=0 read_avg=0 destroy_avg=0
    for t in "${create_times[@]}"; do create_avg=$((create_avg + t)); done
    for t in "${read_times[@]}"; do read_avg=$((read_avg + t)); done
    for t in "${destroy_times[@]}"; do destroy_avg=$((destroy_avg + t)); done
    create_avg=$((create_avg / ITERATIONS))
    read_avg=$((read_avg / ITERATIONS))
    destroy_avg=$((destroy_avg / ITERATIONS))

    echo "${create_avg},${read_avg},${destroy_avg}"
}

format_ms() {
    local ms="$1"
    if [ "$ms" -ge 1000 ]; then
        local secs=$((ms / 1000))
        local remainder=$((ms % 1000 / 100))
        echo "${secs}.${remainder}s"
    else
        echo "${ms}ms"
    fi
}

# ── Benchmark resources ─────────────────────────────────────────────

# S3 Bucket (simplest resource, both providers)
AWS_S3="$TMPDIR/aws_s3.crn"
cat > "$AWS_S3" << 'EOF'
provider aws {
  region = aws.Region.ap_northeast_1
}

aws.s3.bucket {
  bucket = "carina-bench-aws"
  tags = {
    Environment = "benchmark"
  }
}
EOF

AWSCC_S3="$TMPDIR/awscc_s3.crn"
cat > "$AWSCC_S3" << 'EOF'
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.s3.bucket {
  bucket_name = "carina-bench-awscc"
  tags = {
    Environment = "benchmark"
  }

  lifecycle {
    force_delete = true
  }
}
EOF

# EC2 VPC
AWS_VPC="$TMPDIR/aws_vpc.crn"
cat > "$AWS_VPC" << 'EOF'
provider aws {
  region = aws.Region.ap_northeast_1
}

aws.ec2.vpc {
  cidr_block = "10.200.0.0/16"
  tags = {
    Environment = "benchmark"
  }
}
EOF

AWSCC_VPC="$TMPDIR/awscc_vpc.crn"
cat > "$AWSCC_VPC" << 'EOF'
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
  cidr_block = "10.200.0.0/16"
  tags = {
    Environment = "benchmark"
  }
}
EOF

# EC2 Security Group
AWS_SG="$TMPDIR/aws_sg.crn"
cat > "$AWS_SG" << 'EOF'
provider aws {
  region = aws.Region.ap_northeast_1
}

let vpc = aws.ec2.vpc {
  cidr_block = "10.201.0.0/16"
  tags = {
    Environment = "benchmark"
  }
}

aws.ec2.security_group {
  group_name = "carina-bench-sg"
  description = "Benchmark security group"
  vpc_id = vpc.vpc_id
  tags = {
    Environment = "benchmark"
  }
}
EOF

AWSCC_SG="$TMPDIR/awscc_sg.crn"
cat > "$AWSCC_SG" << 'EOF'
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
  cidr_block = "10.201.0.0/16"
  tags = {
    Environment = "benchmark"
  }
}

awscc.ec2.security_group {
  group_name = "carina-bench-sg"
  group_description = "Benchmark security group"
  vpc_id = vpc.vpc_id
  tags = {
    Environment = "benchmark"
  }
}
EOF

# ── Run benchmarks ──────────────────────────────────────────────────

echo "Running S3 Bucket benchmarks..."
echo "  aws provider..."
aws_s3_result=$(run_benchmark "aws" "s3" "$AWS_S3")
echo "  awscc provider..."
awscc_s3_result=$(run_benchmark "awscc" "s3" "$AWSCC_S3")

echo "Running EC2 VPC benchmarks..."
echo "  aws provider..."
aws_vpc_result=$(run_benchmark "aws" "vpc" "$AWS_VPC")
echo "  awscc provider..."
awscc_vpc_result=$(run_benchmark "awscc" "vpc" "$AWSCC_VPC")

echo "Running EC2 Security Group benchmarks..."
echo "  aws provider..."
aws_sg_result=$(run_benchmark "aws" "sg" "$AWS_SG")
echo "  awscc provider..."
awscc_sg_result=$(run_benchmark "awscc" "sg" "$AWSCC_SG")

echo ""

# ── Output results ──────────────────────────────────────────────────

print_row() {
    local resource="$1"
    local operation="$2"
    local aws_ms="$3"
    local awscc_ms="$4"

    local aws_fmt=$(format_ms "$aws_ms")
    local awscc_fmt=$(format_ms "$awscc_ms")

    if [ "$aws_ms" -gt 0 ]; then
        local ratio_x10=$((awscc_ms * 10 / aws_ms))
        local ratio_int=$((ratio_x10 / 10))
        local ratio_frac=$((ratio_x10 % 10))
        local ratio="${ratio_int}.${ratio_frac}x"
    else
        local ratio="N/A"
    fi

    printf "| %-20s | %-10s | %10s | %10s | %6s |\n" "$resource" "$operation" "$aws_fmt" "$awscc_fmt" "$ratio"
}

echo "Results (average of $ITERATIONS iterations)"
echo ""
printf "| %-20s | %-10s | %10s | %10s | %6s |\n" "Resource" "Operation" "aws (SDK)" "awscc (CC)" "Ratio"
printf "|%-22s|%-12s|%12s|%12s|%8s|\n" "$(printf '%0.s-' {1..22})" "$(printf '%0.s-' {1..12})" "$(printf '%0.s-' {1..12})" "$(printf '%0.s-' {1..12})" "$(printf '%0.s-' {1..8})"

IFS=',' read -r aws_c aws_r aws_d <<< "$aws_s3_result"
IFS=',' read -r awscc_c awscc_r awscc_d <<< "$awscc_s3_result"
print_row "S3 Bucket" "Create" "$aws_c" "$awscc_c"
print_row "S3 Bucket" "Read" "$aws_r" "$awscc_r"
print_row "S3 Bucket" "Destroy" "$aws_d" "$awscc_d"

IFS=',' read -r aws_c aws_r aws_d <<< "$aws_vpc_result"
IFS=',' read -r awscc_c awscc_r awscc_d <<< "$awscc_vpc_result"
print_row "EC2 VPC" "Create" "$aws_c" "$awscc_c"
print_row "EC2 VPC" "Read" "$aws_r" "$awscc_r"
print_row "EC2 VPC" "Destroy" "$aws_d" "$awscc_d"

IFS=',' read -r aws_c aws_r aws_d <<< "$aws_sg_result"
IFS=',' read -r awscc_c awscc_r awscc_d <<< "$awscc_sg_result"
print_row "EC2 Security Group" "Create" "$aws_c" "$awscc_c"
print_row "EC2 Security Group" "Read" "$aws_r" "$awscc_r"
print_row "EC2 Security Group" "Destroy" "$aws_d" "$awscc_d"

echo ""
echo "Note: Times include Carina overhead (parsing, diffing, state I/O)."
echo "The ratio shows how much slower awscc (Cloud Control) is compared to aws (SDK)."
