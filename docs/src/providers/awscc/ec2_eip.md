# awscc.ec2_eip

CloudFormation Type: `AWS::EC2::EIP`

Specifies an Elastic IP (EIP) address and can, optionally, associate it with an Amazon EC2 instance.
 You can allocate an Elastic IP address from an address pool owned by AWS or from an address pool created from a public IPv4 address range that you have brought to AWS for use with your AWS resources using bring your own IP addresses (BYOIP). For more information, see [Bring Your Own IP Addresses (BYOIP)](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-byoip.html) in the *Amazon EC2 User Guide*.
 For more information, see [Elastic IP Addresses](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/elastic-ip-addresses-eip.html) in the *Amazon EC2 User Guide*.

## Argument Reference

### `address`

- **Type:** String
- **Required:** No



### `domain`

- **Type:** String
- **Required:** No

The network (``vpc``). If you define an Elastic IP address and associate it with a VPC that is defined in the same template, you must declare a dependency on the VPC-gateway attachment by using the [DependsOn Attribute](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-attribute-dependson.html) on this resource.

### `instance_id`

- **Type:** String
- **Required:** No

The ID of the instance.  Updates to the ``InstanceId`` property may require *some interruptions*. Updates on an EIP reassociates the address on its associated resource.

### `ipam_pool_id`

- **Type:** String
- **Required:** No



### `network_border_group`

- **Type:** String
- **Required:** No

A unique set of Availability Zones, Local Zones, or Wavelength Zones from which AWS advertises IP addresses. Use this parameter to limit the IP address to this location. IP addresses cannot move between network border groups. Use [DescribeAvailabilityZones](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeAvailabilityZones.html) to view the network border groups.

### `public_ipv4_pool`

- **Type:** String
- **Required:** No

The ID of an address pool that you own. Use this parameter to let Amazon EC2 select an address from the address pool.  Updates to the ``PublicIpv4Pool`` property may require *some interruptions*. Updates on an EIP reassociates the address on its associated resource.

### `tags`

- **Type:** Map
- **Required:** No

Any tags assigned to the Elastic IP address.  Updates to the ``Tags`` property may require *some interruptions*. Updates on an EIP reassociates the address on its associated resource.

### `transfer_address`

- **Type:** String
- **Required:** No

The Elastic IP address you are accepting for transfer. You can only accept one transferred address. For more information on Elastic IP address transfers, see [Transfer Elastic IP addresses](https://docs.aws.amazon.com/vpc/latest/userguide/vpc-eips.html#transfer-EIPs-intro) in the *Amazon Virtual Private Cloud User Guide*.

## Attribute Reference

### `allocation_id`

- **Type:** String

### `public_ip`

- **Type:** Ipv4Address



## Example

```crn
awscc.ec2_eip {
  name   = "example-eip"
  domain = "vpc"

  tags = {
    Environment = "example"
  }
}
```
