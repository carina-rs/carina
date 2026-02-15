# awscc.ec2_vpn_gateway

CloudFormation Type: `AWS::EC2::VPNGateway`

Specifies a virtual private gateway. A virtual private gateway is the endpoint on the VPC side of your VPN connection. You can create a virtual private gateway before creating the VPC itself.
 For more information, see [](https://docs.aws.amazon.com/vpn/latest/s2svpn/VPC_VPN.html) in the *User Guide*.

## Argument Reference

### `amazon_side_asn`

- **Type:** Int
- **Required:** No

The private Autonomous System Number (ASN) for the Amazon side of a BGP session.

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the virtual private gateway.

### `type`

- **Type:** String
- **Required:** Yes

The type of VPN connection the virtual private gateway supports.

## Attribute Reference

### `vpn_gateway_id`

- **Type:** VpnGatewayId



## Example

```crn
let vpn_gw = awscc.ec2_vpn_gateway {
  type = "ipsec.1"

  tags = {
    Environment = "example"
  }
}
```
