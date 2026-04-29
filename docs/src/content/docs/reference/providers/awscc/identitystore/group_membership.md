---
title: "awscc.identitystore.GroupMembership"
description: "AWSCC IDENTITYSTORE GroupMembership resource reference"
---


CloudFormation Type: `AWS::IdentityStore::GroupMembership`

Resource Type Definition for AWS:IdentityStore::GroupMembership

## Argument Reference

### `group_id`

- **Type:** String(pattern, len: 1..=47)
- **Required:** Yes
- **Create-only:** Yes

The unique identifier for a group in the identity store.

### `identity_store_id`

- **Type:** String(pattern, len: 1..=36)
- **Required:** Yes
- **Create-only:** Yes

The globally unique identifier for the identity store.

### `member_id`

- **Type:** [Struct(MemberId)](#memberid)
- **Required:** Yes
- **Create-only:** Yes

An object containing the identifier of a group member.

## Struct Definitions

### MemberId

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `user_id` | String(pattern, len: 1..=47) | Yes | The identifier for a user in the identity store. |

## Attribute Reference

### `membership_id`

- **Type:** String(pattern, len: 1..=47)

The identifier for a GroupMembership in the identity store.

