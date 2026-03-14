#!/bin/bash
# Check that every resource type has a corresponding example file
#
# Usage (from project root):
#   ./carina-provider-aws/scripts/check-examples.sh

set -e

EXAMPLES_DIR="carina-provider-aws/examples"

# All resource types supported by the aws provider
# (from carina-provider-aws/src/schemas/generated/mod.rs)
RESOURCE_TYPES=(
    "ec2_internet_gateway"
    "ec2_route"
    "ec2_route_table"
    "ec2_security_group"
    "ec2_security_group_egress"
    "ec2_security_group_ingress"
    "ec2_subnet"
    "ec2_vpc"
    "s3_bucket"
    "sts_caller_identity"
)

MISSING=()

for RESOURCE in "${RESOURCE_TYPES[@]}"; do
    EXAMPLE_FILE="$EXAMPLES_DIR/$RESOURCE/main.crn"
    if [ ! -f "$EXAMPLE_FILE" ]; then
        MISSING+=("$RESOURCE ($EXAMPLE_FILE)")
    fi
done

if [ ${#MISSING[@]} -gt 0 ]; then
    echo "ERROR: Missing example files for the following resource types:"
    for entry in "${MISSING[@]}"; do
        echo "  - $entry"
    done
    echo ""
    echo "Please create example .crn files in $EXAMPLES_DIR/<resource_type>/main.crn"
    exit 1
fi

echo "All resource types have example files."
