---
title: "aws.route53.record_set"
description: "AWS ROUTE53 record_set resource reference"
---


CloudFormation Type: `AWS::Route53::RecordSet`

Information about the resource record set to create or delete.

## Argument Reference

### `alias_target`

- **Type:** [Struct(AliasTarget)](#aliastarget)
- **Required:** No

Alias resource record sets only: Information about the Amazon Web Services resource, such as a CloudFront distribution or an Amazon S3 bucket, that you want to route traffic to. If you're creating resource records sets for a private hosted zone, note the following: You can't create an alias resource record set in a private hosted zone to route traffic to a CloudFront distribution. For information about creating failover resource record sets in a private hosted zone, see Configuring Failover in a Private Hosted Zone in the Amazon Route 53 Developer Guide.

### `name`

- **Type:** String
- **Required:** Yes

For ChangeResourceRecordSets requests, the name of the record that you want to create, update, or delete. For ListResourceRecordSets responses, the name of a record in the specified hosted zone. ChangeResourceRecordSets Only Enter a fully qualified domain name, for example, www.example.com. You can optionally include a trailing dot. If you omit the trailing dot, Amazon Route 53 assumes that the domain name that you specify is fully qualified. This means that Route 53 treats www.example.com (without a trailing dot) and www.example.com. (with a trailing dot) as identical. For information about how to specify characters other than a-z, 0-9, and - (hyphen) and how to specify internationalized domain names, see DNS Domain Name Format in the Amazon Route 53 Developer Guide. You can use the asterisk (*) wildcard to replace the leftmost label in a domain name, for example, *.example.com. Note the following: The * must replace the entire label. For example, you can't specify *prod.example.com or prod*.example.com. The * can't replace any of the middle labels, for example, marketing.*.example.com. If you include * in any position other than the leftmost label in a domain name, DNS treats it as an * character (ASCII 42), not as a wildcard. You can't use the * wildcard for resource records sets that have a type of NS.

### `resource_records`

- **Type:** AttributeType::list(AttributeType::String)
- **Required:** No

Information about the resource records to act upon. If you're creating an alias resource record set, omit ResourceRecords.

### `ttl`

- **Type:** Int(0..=2147483647)
- **Required:** No

The resource record cache time to live (TTL), in seconds. Note the following: If you're creating or updating an alias resource record set, omit TTL. Amazon Route 53 uses the value of TTL for the alias target. If you're associating this resource record set with a health check (if you're adding a HealthCheckId element), we recommend that you specify a TTL of 60 seconds or less so clients respond quickly to changes in health status. All of the resource record sets in a group of weighted resource record sets must have the same value for TTL. If a group of weighted resource record sets includes one or more weighted alias resource record sets for which the alias target is an ELB load balancer, we recommend that you specify a TTL of 60 seconds for all of the non-alias weighted resource record sets that have the same name and type. Values other than 60 seconds (the TTL for load balancers) will change the effect of the values that you specify for Weight.

### `type`

- **Type:** [Enum (Type)](#type-type)
- **Required:** Yes

The DNS record type. For information about different record types and how data is encoded for them, see Supported DNS Resource Record Types in the Amazon Route 53 Developer Guide. Valid values for basic resource record sets: A | AAAA | CAA | CNAME | DS |MX | NAPTR | NS | PTR | SOA | SPF | SRV | TXT| TLSA| SSHFP| SVCB| HTTPS Values for weighted, latency, geolocation, and failover resource record sets: A | AAAA | CAA | CNAME | MX | NAPTR | PTR | SPF | SRV | TXT| TLSA| SSHFP| SVCB| HTTPS. When creating a group of weighted, latency, geolocation, or failover resource record sets, specify the same value for all of the resource record sets in the group. Valid values for multivalue answer resource record sets: A | AAAA | MX | NAPTR | PTR | SPF | SRV | TXT| CAA| TLSA| SSHFP| SVCB| HTTPS SPF records were formerly used to verify the identity of the sender of email messages. However, we no longer recommend that you create resource record sets for which the value of Type is SPF. RFC 7208, Sender Policy Framework (SPF) for Authorizing Use of Domains in Email, Version 1, has been updated to say, "...[I]ts existence and mechanism defined in [RFC4408] have led to some interoperability issues. Accordingly, its use is no longer appropriate for SPF version 1; implementations are not to use it." In RFC 7208, see section 14.1, The SPF DNS Record Type. Values for alias resource record sets: Amazon API Gateway custom regional APIs and edge-optimized APIs: A CloudFront distributions: A If IPv6 is enabled for the distribution, create two resource record sets to route traffic to your distribution, one with a value of A and one with a value of AAAA. Amazon API Gateway environment that has a regionalized subdomain: A ELB load balancers: A | AAAA Amazon S3 buckets: A Amazon Virtual Private Cloud interface VPC endpoints A Another resource record set in this hosted zone: Specify the type of the resource record set that you're creating the alias for. All values are supported except NS and SOA. If you're creating an alias record that has the same name as the hosted zone (known as the zone apex), you can't route traffic to a record for which the value of Type is CNAME. This is because the alias record must have the same type as the record you're routing traffic to, and creating a CNAME record for the zone apex isn't supported even for an alias record.

### `hosted_zone_id`

- **Type:** String
- **Required:** No

The ID of the hosted zone that contains this record set.

## Enum Values

### type (Type)

| Value | DSL Identifier |
|-------|----------------|
| `A` | `aws.route53.record_set.Type.A` |
| `AAAA` | `aws.route53.record_set.Type.AAAA` |
| `CAA` | `aws.route53.record_set.Type.CAA` |
| `CNAME` | `aws.route53.record_set.Type.CNAME` |
| `DS` | `aws.route53.record_set.Type.DS` |
| `HTTPS` | `aws.route53.record_set.Type.HTTPS` |
| `MX` | `aws.route53.record_set.Type.MX` |
| `NAPTR` | `aws.route53.record_set.Type.NAPTR` |
| `NS` | `aws.route53.record_set.Type.NS` |
| `PTR` | `aws.route53.record_set.Type.PTR` |
| `SOA` | `aws.route53.record_set.Type.SOA` |
| `SPF` | `aws.route53.record_set.Type.SPF` |
| `SRV` | `aws.route53.record_set.Type.SRV` |
| `SSHFP` | `aws.route53.record_set.Type.SSHFP` |
| `SVCB` | `aws.route53.record_set.Type.SVCB` |
| `TLSA` | `aws.route53.record_set.Type.TLSA` |
| `TXT` | `aws.route53.record_set.Type.TXT` |

Shorthand formats: `A` or `Type.A`

## Struct Definitions

### AliasTarget

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `dns_name` | String | Yes | Alias resource record sets only: The value that you specify depends on where you want to route queri... |
| `evaluate_target_health` | Bool | Yes | Applies only to alias, failover alias, geolocation alias, latency alias, and weighted alias resource... |
| `hosted_zone_id` | String | Yes | Alias resource records sets only: The value used depends on where you want to route traffic: Amazon ... |

