#!/bin/bash
# Check that every resource type has a corresponding example file
#
# Usage (from project root):
#   ./carina-provider-awscc/scripts/check-examples.sh
#
# This script uses the codegen binary to convert CloudFormation type names
# to directory names, matching the same logic used by generate-docs.sh.

set -e

EXAMPLES_DIR="carina-provider-awscc/examples"

# Same resource types as generate-schemas.sh / generate-docs.sh
RESOURCE_TYPES=(
    "AWS::EC2::VPC"
    "AWS::EC2::Subnet"
    "AWS::EC2::InternetGateway"
    "AWS::EC2::RouteTable"
    "AWS::EC2::Route"
    "AWS::EC2::SubnetRouteTableAssociation"
    "AWS::EC2::EIP"
    "AWS::EC2::NatGateway"
    "AWS::EC2::SecurityGroup"
    "AWS::EC2::SecurityGroupIngress"
    "AWS::EC2::SecurityGroupEgress"
    "AWS::EC2::VPCEndpoint"
    "AWS::EC2::VPCGatewayAttachment"
    "AWS::EC2::FlowLog"
    "AWS::EC2::IPAM"
    "AWS::EC2::IPAMPool"
    "AWS::EC2::VPNGateway"
    "AWS::EC2::TransitGateway"
    "AWS::EC2::VPCPeeringConnection"
    "AWS::EC2::EgressOnlyInternetGateway"
    "AWS::EC2::TransitGatewayAttachment"
    "AWS::S3::Bucket"
    "AWS::IAM::Role"
    "AWS::Logs::LogGroup"
)

# Build codegen tool
CODEGEN_BIN="target/debug/codegen"
cargo build -p carina-provider-awscc --bin codegen --quiet 2>/dev/null || {
    echo "ERROR: Could not build codegen binary"
    exit 1
}

MISSING=()

for TYPE_NAME in "${RESOURCE_TYPES[@]}"; do
    FULL_RESOURCE=$("$CODEGEN_BIN" --type-name "$TYPE_NAME" --print-full-resource-name)
    EXAMPLE_FILE="$EXAMPLES_DIR/$FULL_RESOURCE/main.crn"
    if [ ! -f "$EXAMPLE_FILE" ]; then
        MISSING+=("$TYPE_NAME ($EXAMPLE_FILE)")
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
