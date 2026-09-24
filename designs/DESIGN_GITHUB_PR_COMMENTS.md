# Design: Posting Pull Request Review Summaries to GitHub

## 1. Overview

This document completes Phase P7 of
[DESIGN_MULTI_PROJECT_REVIEW.md](DESIGN_MULTI_PROJECT_REVIEW.md): once
the Sashiko instance finishes reviewing a pull request, it posts a
single summary comment back to the pull request thread on GitHub as
`sashiko[bot]`.

The scope here is deliberately **one summary comment per pull request
version** (Option A). Diff-anchored review threads on individual lines
are a separate follow-up and layer cleanly on top of this table and
worker without replacing them.

### 1.1 Goals

1. Post from a **GitHub App** so comments appear under `sashiko[bot]`,
   not a personal account, and so credentials are scoped to a single
   repository installation with no user seat required. Keep a plain
   `api_token` fallback for local testing and forges without an App.
2. **Always post** when a pull request review completes — a clean pull
   request gets a one-line confirmation with a link to the full trace on
   `sashiko.sashiko.dev`, so contributors know the bot ran.
3. Follow the existing outbox pattern (`email_outbox`,
   `patchwork_outbox`): the reviewer writes a row inside the completion
   path and a background worker delivers it with retries and an
   idempotency guard.
4. Default to `post_mode = "off"` so an upgrade posts nothing until an
   operator turns it on explicitly; support `"dry_run"` (compose and
   log, mark `Dry-Run`, never call GitHub) and `"live"`.
5. Zero behaviour change for the Linux instance (`forge.enabled = false`
   means the outbox is never written and the worker exits immediately).

---

## 2. GitHub App Authentication

A GitHub App authenticates in two steps, and every crate needed for both
steps is already compiled into the binary (`jsonwebtoken = "11.0"` with
`EncodingKey::from_rsa_pem` and `Algorithm::RS256`, plus `reqwest` with
`json`):

```
App private key (PEM) + App ID
        │  RS256 JWT (iat = now - 60s, exp = now + 9m, iss = app_id)
        ▼
POST https://api.github.com/app/installations/{installation_id}/access_tokens
        │
        ▼
installation token (valid 1 hour, cached in memory until 5 min before expiry)
        │
        ▼
POST https://api.github.com/repos/{owner}/{repo}/issues/{pr}/comments
```

`ForgeSettings` in `src/settings.rs` gains four optional fields (all
`#[serde(default)]` so existing configs keep parsing under
`deny_unknown_fields`):

```toml
[forge]
enabled = true
provider = "github"
post_mode = "off"          # "off" | "dry_run" | "live"
app_id = 123456
installation_id = 12345678
app_private_key = "-----BEGIN RSA PRIVATE KEY-----\n..."
# or app_private_key_path = "/var/run/secrets/github-app.pem"
api_token = ""             # fallback when app_id is not set
```

The private key is read from `SASHIKO__FORGE__APP_PRIVATE_KEY` (or the
path); it is **never** written to the database or echoed in logs. If
both `app_id` and `api_token` are set, the App wins.

### 2.1 Why `issues/{pr}/comments` and not `pulls/{pr}/reviews`

Both endpoints post a top-level comment into the PR conversation
timeline. `issues/{pr}/comments` needs only the **Pull requests: Read &
write** permission on the GitHub App and does not create a review state
(`APPROVED` / `CHANGES_REQUESTED` / `COMMENTED`) that can interact with
branch protection rules. When diff-anchored comments are added later,
only the HTTP call inside the worker changes.

---

## 3. Outbox Schema (Migration `010_forge_outbox.sql`)

```sql
CREATE TABLE IF NOT EXISTS forge_outbox (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    repo TEXT NOT NULL,            -- "owner/name"
    pr_number INTEGER NOT NULL,
    head_sha TEXT,
    body TEXT NOT NULL,
    target_url TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Pending',
        -- Pending | Sending | Sent | Dry-Run | Disabled | Failed
    locked_at INTEGER,
    retries INTEGER NOT NULL DEFAULT 0,
    error_log TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id)
);
CREATE INDEX IF NOT EXISTS idx_forge_outbox_status ON forge_outbox(status);
```

**Why `UNIQUE(patchset_id)` is the right idempotency key:** each push to
a pull request creates a distinct patchset row (`mr-<n>-<base>..<head>`),
so version 1 and version 2 get distinct `patchset_id`s and each receives
its own comment (`### Sashiko review — v1`, `### Sashiko review — v2`),
while a retry or duplicate completion call on the *same* version hits
`SELECT 1 FROM forge_outbox WHERE patchset_id = ?` and is skipped.

---

## 4. Comment Format

Composed in pure Rust (`src/forge_comment.rs`), unit-tested without an
LLM or network:

**Clean PR (zero findings across all commits):**

```markdown
### Sashiko review — v2

✓ **No issues found** across 3 commits (`a1b2c3d..f4e5d6a`).

[Full review log on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/sashiko-515)
```

**PR with findings:**

```markdown
### Sashiko review — v2

**2 findings** (1 high, 1 medium) across 3 commits (`a1b2c3d..f4e5d6a`).

<details>
<summary>Summary</summary>

The series adds X and refactors Y. One high-severity issue in commit 2 ...
</details>

#### Commit 2/3 — `8ab6e2bc` forge: add version tags and slug rotation

- **[HIGH]** `src/db.rs:5870` — `rotate_mr_slug` overwrites any row ...
- **[MEDIUM]** `src/forge.rs:408` — `extract_repo_name_from_url` drops ...

[Full review and stage logs on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/sashiko-515)
```

Body is hard-capped at 60 000 bytes (GitHub's limit is 65 536); if
truncated, the trailing line points at the web UI where the full output
lives.

---

## 5. Enqueue & Worker Flow

### 5.1 Enqueue (`src/reviewer.rs`)

Right after `update_patchset_status(patchset_id, "Reviewed")` at
`reviewer.rs:748`:

1. Check `review_success`, `patchset.mr_url.is_some()`, and
   `patchset.mr_number.is_some()`. If not a forge PR, return.
2. Parse `owner/repo` from `patchset.mr_url`
   (`https://github.com/{owner}/{repo}/pull/{n}`).
3. Load per-patch summaries and inline reviews for `patchset_id` from
   the DB, compose the markdown body and `target_url`
   (`{public_base_url}/#/patchset/{slug}`).
4. Map `settings.forge.post_mode` (`Off` → `"Disabled"`, `DryRun` →
   `"Dry-Run"`, `Live` → `"Pending"`, or `"Embargoed"` if
   `patchset.embargo_until > now`).
5. `db.insert_forge_outbox(...)` (idempotent on `patchset_id`).

`release_patchset_results` flips any `Embargoed` row for that patchset
to `Pending` alongside the email outbox rows.

### 5.2 Worker (`src/worker/forge.rs`)

Modelled directly on `PatchworkWorker` (`src/worker/patchwork.rs`):

- Spawned from `main.rs` only when `settings.forge.enabled`.
- Every 5 seconds: recover stale `Sending` locks older than 5 minutes,
  claim one `Pending` row (`status = 'Sending', locked_at = now`), mint
  or reuse the cached GitHub App installation token (or read
  `api_token`), `POST` the comment.
- On 2xx: `mark_forge_sent(id)`.
- On 4xx other than 401/403/429 (e.g. 404 deleted PR, 422 locked issue):
  permanent failure, `mark_forge_failed(id, err)` immediately without
  burning retries.
- On 401: invalidate the cached installation token and retry.
- On network error / 5xx / 429: increment `retries`; if `retries >= 3`
  mark `Failed`, otherwise unlock back to `Pending` with exponential
  backoff (5s, 30s, 180s).

---

## 6. Implementation Steps (each a self-contained commit)

| # | Commit | Touches |
|---|---|---|
| 1 | `designs: specify pull request comment delivery for github` | `designs/DESIGN_GITHUB_PR_COMMENTS.md` |
| 2 | `db: add forge_outbox table and queue operations` | `src/migrations/010_forge_outbox.sql`, `src/db.rs` (+ unit tests) |
| 3 | `forge: compose markdown review summaries and github app tokens` | `src/settings.rs`, `src/forge.rs` (App JWT + token exchange + `post_pr_comment` + markdown composer, unit tests with a mock server / canned RSA key) |
| 4 | `worker: deliver forge_outbox comments to github` | `src/worker/forge.rs`, `src/worker/mod.rs`, `src/main.rs`, `src/reviewer.rs` (enqueue on `Reviewed` + embargo release) |

---

## 7. Operational Rollout on `sashiko.sashiko.dev`

1. Create a GitHub App under the `sashiko-dev` org:
   - **Homepage URL:** `https://sashiko.sashiko.dev`
   - **Webhook:** can reuse the existing repo webhook or point the App's
     webhook at `https://sashiko.sashiko.dev/api/webhook/github` with
     the same secret
   - **Permissions:** Repository → *Pull requests: Read and write*
     (automatically grants *Issues: Read and write* on PRs for comments)
   - Install the App on `sashiko-dev/sashiko`; record the **App ID**,
     **Installation ID** (from the installation URL), and generate a
     **private key PEM**.
2. Add to `sashiko-self-secrets`:
   `SASHIKO__FORGE__APP_ID`, `SASHIKO__FORGE__INSTALLATION_ID`,
   `SASHIKO__FORGE__APP_PRIVATE_KEY`.
3. Deploy first with `SASHIKO__FORGE__POST_MODE=dry_run`; verify rows in
   `forge_outbox` land as `Dry-Run` with the rendered markdown in
   `body`.
4. Flip `SASHIKO__FORGE__POST_MODE=live` and re-queue one row (`UPDATE
   forge_outbox SET status='Pending' WHERE id=1`) to confirm the
   `sashiko[bot]` comment appears on the PR.
