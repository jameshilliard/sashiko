# Design: ACL Blocklist Implementation

## Status
Approved

## Context & Motivation
Sashiko utilizes a capability-based Access Control List (ACL) to restrict API mutations (`ingest`, `cancel`, `review`, `action`, and `admins`). While capabilities provide fine-grained, least-privilege access grants, administrators require a mechanism to explicitly deny and revoke access for specific identities immediately.

Use cases include:
- Compromised credentials or compromised service accounts.
- Rapid containment of misbehaving automated bots or malicious actors.
- Disallowing specific users who might otherwise have capabilities configured.

A blocklist provides an immediate, unilateral override across all authorization pathways.

## Design Decisions

### 1. Unconditional Precedence (Anti-Charity & Fail-Closed)
The blocklist evaluation occurs before any capability or admin checks:
1. If an email is present in `blocklist`, `has_permission` MUST return `false` immediately.
2. The blocklist check is case-insensitive (ASCII case-folded) so that casing differences (`Attacker@example.com` vs `attacker@example.com`) cannot bypass enforcement.
3. If an identity appears in both `admins` and `blocklist`, the `blocklist` rule wins.

### 2. Configuration Schema
In `src/settings.rs`:
```rust
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct AclSettings {
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default)]
    pub ingest: Vec<String>,
    #[serde(default)]
    pub cancel: Vec<String>,
    #[serde(default)]
    pub review: Vec<String>,
    #[serde(default)]
    pub action: Vec<String>,
    #[serde(default)]
    pub blocklist: Vec<String>,
}
```

In `Settings.toml`:
```toml
[server.acl]
# Admins have all capabilities, including root-level maintenance operations.
admins = ["admin@example.com"]
ingest = []
cancel = []
review = []
action = []
# Blocklist explicitly denies all access, overriding any capability grants.
blocklist = []
```

### 3. API & Authentication Flow Enforcement
Enforcement spans all authentication and authorization touchpoints in `src/api.rs`:
1. `is_authorized`: Checks if the authenticated user's email is blocklisted; if so, authorization is rejected.
2. `request_link`: Disallows sending sign-in links to blocklisted addresses (`403 Forbidden`).
3. `verify_link`: Disallows exchanging a sign-in link for a session token if the claims subject is blocklisted (`403 Forbidden`).
4. `refresh_token`: Disallows issuing a new session token if the authenticated user is blocklisted (`403 Forbidden`).

### 4. Verification & Testing
- Unit tests in `src/settings.rs`:
  - Verify `is_blocklisted` logic (empty list, positive match, negative match, case-insensitivity).
  - Verify `has_permission` denial when blocklisted for admin and all `Permission` variants.
  - Verify deserialization of `Settings.toml`.
- API tests in `src/api.rs`:
  - Verify `request_link` rejection for blocklisted identity.
  - Verify `verify_link` rejection for blocklisted identity.
  - Verify `refresh_token` rejection for blocklisted identity.
  - Verify `is_authorized` denial for blocklisted identity.
