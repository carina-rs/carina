---
title: "aws.identitystore.user"
description: "AWS IDENTITYSTORE user resource reference"
---


CloudFormation Type: `AWS::IdentityStore::User`

This is a **data source** (read-only). Use with the `read` keyword.

## Lookup Inputs

### `identity_store_id`

- **Required:** Yes

The globally unique identifier for the identity store.

### `user_id`

- **Required:** No

The identifier for the user. Provide either user_id or user_name.

### `user_name`

- **Required:** No

The user's user name. Provide either user_id or user_name.

## Attributes

### `display_name`

- **Type:** String
- **Read-only**

<p>The display name of the user.</p>

### `emails`

- **Type:** String
- **Read-only**

<p>The email address of the user.</p>

