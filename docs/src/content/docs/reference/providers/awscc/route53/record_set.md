---
title: "awscc.route53.record_set"
description: "AWSCC ROUTE53 record_set resource reference"
---


CloudFormation Type: `AWS::Route53::RecordSet`

Resource Type definition for AWS::Route53::RecordSet

## Argument Reference

### `alias_target`

- **Type:** [Struct(AliasTarget)](#aliastarget)
- **Required:** No

### `cidr_routing_config`

- **Type:** [Struct(CidrRoutingConfig)](#cidrroutingconfig)
- **Required:** No

### `comment`

- **Type:** String
- **Required:** No

### `failover`

- **Type:** String
- **Required:** No

### `geo_location`

- **Type:** [Struct(GeoLocation)](#geolocation)
- **Required:** No

### `geo_proximity_location`

- **Type:** [Struct(GeoProximityLocation)](#geoproximitylocation)
- **Required:** No

### `health_check_id`

- **Type:** String
- **Required:** No

### `hosted_zone_id`

- **Type:** String
- **Required:** No
- **Create-only:** Yes

### `hosted_zone_name`

- **Type:** String
- **Required:** No
- **Create-only:** Yes

### `multi_value_answer`

- **Type:** Bool
- **Required:** No

### `name`

- **Type:** String
- **Required:** Yes
- **Create-only:** Yes

### `region`

- **Type:** Region
- **Required:** No

### `resource_records`

- **Type:** `List<String>`
- **Required:** No

### `set_identifier`

- **Type:** String
- **Required:** No

### `ttl`

- **Type:** String
- **Required:** No

### `type`

- **Type:** String
- **Required:** Yes

### `weight`

- **Type:** Int
- **Required:** No

## Struct Definitions

### AliasTarget

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `dns_name` | String | Yes |  |
| `evaluate_target_health` | Bool | No |  |
| `hosted_zone_id` | String | Yes |  |

### CidrRoutingConfig

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `collection_id` | String | Yes |  |
| `location_name` | String | Yes |  |

### Coordinates

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `latitude` | String | Yes |  |
| `longitude` | String | Yes |  |

### GeoLocation

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `continent_code` | String | No |  |
| `country_code` | String | No |  |
| `subdivision_code` | String | No |  |

### GeoProximityLocation

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `aws_region` | Region | No |  |
| `bias` | Int | No |  |
| `coordinates` | [Struct(Coordinates)](#coordinates) | No |  |
| `local_zone_group` | String | No |  |

## Attribute Reference

### `id`

- **Type:** String

