---
title: "aws.ec2.TransitGateway"
description: "AWS EC2 transit_gateway resource reference"
---


CloudFormation Type: `AWS::EC2::TransitGateway`

Describes a transit gateway.

## Argument Reference

### `description`

- **Type:** String
- **Required:** No

A description of the transit gateway.

### `options`

- **Type:** [Struct(TransitGatewayRequestOptions)](#transitgatewayrequestoptions)
- **Required:** No

The transit gateway options.

### `tags`

- **Type:** Map
- **Required:** No

The tags for the resource.

## Struct Definitions

### TransitGatewayRequestOptions

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `amazon_side_asn` | Int | No | A private Autonomous System Number (ASN) for the Amazon side of a BGP session. The range is 64512 to... |
| `auto_accept_shared_attachments` | [Enum (AutoAcceptSharedAttachments)](#auto_accept_shared_attachments-autoacceptsharedattachments) | No | Enable or disable automatic acceptance of attachment requests. Disabled by default. |
| `default_route_table_association` | [Enum (DefaultRouteTableAssociation)](#default_route_table_association-defaultroutetableassociation) | No | Enable or disable automatic association with the default association route table. Enabled by default... |
| `default_route_table_propagation` | [Enum (DefaultRouteTablePropagation)](#default_route_table_propagation-defaultroutetablepropagation) | No | Enable or disable automatic propagation of routes to the default propagation route table. Enabled by... |
| `dns_support` | [Enum (DnsSupport)](#dns_support-dnssupport) | No | Enable or disable DNS support. Enabled by default. |
| `multicast_support` | [Enum (MulticastSupport)](#multicast_support-multicastsupport) | No | Indicates whether multicast is enabled on the transit gateway |
| `security_group_referencing_support` | [Enum (SecurityGroupReferencingSupport)](#security_group_referencing_support-securitygroupreferencingsupport) | No | Enables you to reference a security group across VPCs attached to a transit gateway to simplify secu... |
| `transit_gateway_cidr_blocks` | `List<Ipv4Cidr>` | No | One or more IPv4 or IPv6 CIDR blocks for the transit gateway. Must be a size /24 CIDR block or large... |
| `vpn_ecmp_support` | [Enum (VpnEcmpSupport)](#vpn_ecmp_support-vpnecmpsupport) | No | Enable or disable Equal Cost Multipath Protocol support. Enabled by default. |

## Attribute Reference

### `transit_gateway_id`

- **Type:** transit_gateway_id

