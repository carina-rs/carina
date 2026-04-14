---
title: "awscc.identitystore.group"
description: "AWSCC IDENTITYSTORE group resource reference"
---


CloudFormation Type: `AWS::IdentityStore::Group`

Resource Type definition for AWS::IdentityStore::Group

## Argument Reference

### `description`

- **Type:** String(pattern, len: 1..=1024)
- **Required:** No

A string containing the description of the group.

### `display_name`

- **Type:** String(pattern, len: 1..=1024)
- **Required:** Yes

A string containing the name of the group. This value is commonly displayed when the group is referenced.

### `identity_store_id`

- **Type:** String(pattern, len: 1..=36)
- **Required:** Yes
- **Create-only:** Yes

The globally unique identifier for the identity store.

## Attribute Reference

### `group_id`

- **Type:** String(pattern, len: 1..=47)

The unique identifier for a group in the identity store.

