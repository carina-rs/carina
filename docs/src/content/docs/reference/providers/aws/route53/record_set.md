---
title: "aws.route53.record_set"
description: "AWS ROUTE53 record_set resource reference"
---


CloudFormation Type: `AWS::Route53::RecordSet`

Information about the resource record set to create or delete.

## Argument Reference

### `change_batch`

- **Type:** [Struct(ChangeBatch)](#changebatch)
- **Required:** Yes

A complex type that contains an optional comment and the Changes element.

### `hosted_zone_id`

- **Type:** String
- **Required:** Yes

The ID of the hosted zone that contains this record set.

### `hosted_zone_id`

- **Type:** String
- **Required:** No

The ID of the hosted zone that contains this record set.

## Struct Definitions

### ChangeBatch

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `changes` | `List<[Struct(Change)](#change)>` | Yes | Information about the changes to make to the record sets. |
| `comment` | String | No | Optional: Any comments you want to include about a change batch request. |

## Attribute Reference

### `name`

- **Type:** String

