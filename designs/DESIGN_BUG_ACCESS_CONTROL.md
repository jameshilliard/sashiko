# Design: Linux Bug Access Control and Login Hardening

## Status

Proposed. This document is a release blocker for the `database` branch: the
branch introduces the `bugs` tables and a full HTTP surface over them,
and today every one of those read routes is completely unauthenticated.

## Context and Motivation

The `database` branch adds a bug tracker for pre-existing Linux kernel defects
discovered by Sashiko's analysis workflows. Those records are, by construction,
descriptions of unfixed vulnerabilities in shipping kernels together with the
reasoning and reproduction traces that led to them. They are pre-disclosure
material and must not be world-readable.

The branch as it stands does not treat them that way:

* All eight bug read routes (`/api/bug`, `/api/bugs`, `/api/bugs/subsystems`,
  `/api/subsystems`, `/api/bug/logs`, `/api/bug/raw`, `/api/bug/input`,
  `/api/bug/enrichments`) have no authorization check at all.
* `/api/bug/action` checks a single global `Permission::Action` grant, which is
  all-or-nothing across every bug, and it checks it *before* the bug is loaded,
  so the check cannot depend on which bug is being mutated.
* `/api/bug/analyze`, the only route that creates a bug, is gated on
  `Permission::Review`, conflating "may re-run a patch review" with "may file a
  kernel vulnerability report".
* The operator bypass in `is_authorized`, which admits a caller that presents no
  session at all, applies to bug routes like everything else.
* The sign-in link login that would back any of this is not actually a login: the
  link is printed to the server log rather than mailed, the request endpoint is an
  email-enumeration oracle, and sessions can be refreshed forever.

`DESIGN_ROLE_BASED_ACCESS_CONTROL.md` sketched an ambitious model with a
`users` table, role grants, personal access tokens and embargo states. None of
it shipped, and most of it is more machinery than this problem needs. This
document specifies the model that will actually ship, derived from the
operational reality that **the authoritative list of who owns what in the Linux
kernel already exists and is already parsed by Sashiko**: the MAINTAINERS file.

## Goals

1. Bugs are private. No anonymous read, ever, on any bug route.
2. A kernel subsystem maintainer can see and manage the bugs filed against
   their own subsystems, and nothing else, with no manual provisioning.
3. The kernel security list can see and comment on every bug, but cannot
   unilaterally close or dismiss bugs in subsystems they do not maintain.
4. Sashiko's own operators have full access.
5. Automation can file bugs and nothing more, with room to widen later.
6. Login is a real email login: unguessable, stateless with a 30-minute link
   expiration, rate limited, and incapable of revealing who is eligible to log
   in.

## Non-Goals

* A `users` table, a role-grant UI, or personal access tokens. Authority is
  derived, not provisioned.
* An email alias map. A person who appears in MAINTAINERS under more than one
  address is treated as more than one principal, and each address carries only
  the authority MAINTAINERS and the configured lists give it.
* Per-bug embargo states. Every bug is private; there is no second tier.
* Splitting this into a separate pull request. It merges atomically with the
  rest of the branch.

---

## Part 1: The Authority Model

### 1.1 Three sources of authority, composed additively

| Source | Where it comes from | What it grants |
| :--- | :--- | :--- |
| Configured capability lists | `[server.acl]` in `Settings.toml` | Global roles: operator, security list, bug reporter |
| MAINTAINERS-derived scope | `third_party/linux/MAINTAINERS`, parsed at startup | Management authority over specific subsystems |
| Local operator token | `.sashiko-local-token`, readable only by the user the server runs as | Everything **except** bug routes |

A principal's effective authority is the union of what each source grants. This
matters for the concrete case that motivated it: Greg Kroah-Hartman sits on the
security list *and* maintains eighteen subsystems. Union semantics give him
comment authority everywhere plus full management authority in his own trees,
without granting him the operator capabilities (`ingest`, `cancel`, `review`)
that putting him in `admins` would.

### 1.2 Access levels

Access to a bug is a total order, so "the union of two grants" is simply the
maximum. This is expressed as an ordered enum rather than a set of booleans so
that the composition is a `max` and invalid combinations are unrepresentable.

```rust
/// What a principal may do to one specific bug.
///
/// Ordered from least to most authority. Composing two grants over the same
/// bug takes the maximum, so a security-list member who also maintains the
/// affected subsystem gets Manage, not Comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BugAccess {
    /// The bug is invisible. Reads return 404, not 403, so that the existence
    /// of a bug is not itself disclosed.
    None,
    /// May read the bug, its report, its enrichments and its comments.
    /// Deliberately does NOT include the raw AI transcripts; see 1.4a.
    Read,
    /// Read, plus may attach a comment.
    Comment,
    /// Comment, plus may close, dismiss, assign and mark duplicate.
    ///
    /// There is no bug re-analysis endpoint today. When one is added it
    /// belongs here and not at Comment, because re-running spends money and
    /// the security list must not be able to trigger it outside its own
    /// maintained subsystems.
    Manage,
}
```

`Read` exists as a distinct level even though no principal currently stops
there, because it is the natural floor for a future read-only integration and
because expressing "comment implies read" as an ordering is clearer than
repeating the implication at every call site.

### 1.3 The principal

```rust
/// The bug-domain authority of one authenticated caller, resolved once per
/// request from configuration and the MAINTAINERS index.
pub struct BugPrincipal {
    email: String,
    /// Sashiko operator: Manage on every bug.
    operator: bool,
    /// Kernel security list member: Comment on every bug.
    security: bool,
    /// Manage on every bug, granted to maintainers of a catch-all MAINTAINERS
    /// section (see 1.5).
    global_maintainer: bool,
    /// MAINTAINERS section titles this address maintains. Manage within these.
    maintained_sections: Arc<HashSet<SectionTitle>>,
    /// May submit new bugs via /api/bug/analyze.
    may_create: bool,
}

impl BugPrincipal {
    /// Resolves the access level for a bug given the subsystems attributed to
    /// it, considering only subsystems whose provenance is a MAINTAINERS
    /// section title (see 2.1).
    pub fn access_to(&self, attributed: &[SectionTitle]) -> BugAccess;

    /// True when this principal can read every bug regardless of subsystem.
    /// Gates the raw analysis transcripts, which disclose other bugs; see
    /// 1.4a for why the two are separate.
    pub fn has_global_bug_visibility(&self) -> bool;
}
```

`SectionTitle` is a newtype over `String` with a normalizing constructor
(trim, collapse internal whitespace, compare case-insensitively). It exists to
make it a type error to accidentally compare a MAINTAINERS section title
against a directory prefix, which is the single most likely way to get this
wrong.

### 1.4 The resolution rules

```
access_to(bug_subsystems):
    if operator or global_maintainer:            Manage
    else if maintained_sections ∩ bug_subsystems ≠ ∅:  Manage
    else if security:                            Comment
    else:                                        None
```

Read the consequences off that table:

* A bug attributed to `BTRFS FILE SYSTEM` is manageable by the Btrfs
  maintainers, commentable by the security list, and invisible to everyone
  else.
* A bug with **no** attributable subsystems, or one attributed only to a
  directory prefix or to the literal `kernel` sentinel, matches no maintainer's
  section set. It therefore falls through to `Comment` for the security list
  and `Manage` for operators, and is invisible to all maintainers. This is the
  intended fail-closed behavior for unclassifiable bugs.
* A bug attributed to several subsystems is manageable by the maintainers of
  *any* of them. Kernel practice is that a cross-subsystem bug is every
  affected maintainer's business.

### 1.4a Raw transcripts are separate, and narrower

`BugAccess` governs the bug: its report, enrichments and comments. It does
**not** govern the raw AI transcripts served by `/api/bug/logs`,
`/api/bug/raw` and `/api/bug/input`. Those need a stricter rule, because they
leak across subsystem boundaries by construction.

The deduplication stage selects candidates from
[`list_all_bugs_for_vector_search()`](file:///usr/local/google/home/kfree/sashiko/src/workflows/linux_bug.rs#L1965),
which is every bug in the database with no scope filter, and the prompt embeds
each candidate's bug id, severity, subsystems, **problem statement** and
affected files
([linux_bug.rs:607-615](file:///usr/local/google/home/kfree/sashiko/src/workflows/linux_bug.rs#L607-L615)).
That interaction is recorded against the bug and served verbatim. A Btrfs
maintainer reading their own bug's transcript would therefore see the problem
statements of unrelated bugs. Vector similarity usually keeps candidates in the
same subsystem, but nothing enforces it, and an access model that depends on a
similarity threshold is not an access model.

The rule is therefore:

```
may_read_transcripts = operator or security or global_maintainer
```

which is exactly the set of principals who can already read every bug. For
them the transcript discloses nothing they could not fetch directly, so the
restriction is derived rather than arbitrary. It is expressed as
`has_global_bug_visibility()` on the principal, not as a fourth `BugAccess`
level, because it is a property of the caller and not of the bug.

Filtering dedup candidates by subsystem at generation time was considered and
rejected: it would weaken cross-subsystem deduplication, which is a product
regression in service of an access-control problem better solved at the edge.

The same reasoning applies to the duplicate link. A bug marked duplicate names
its canonical bug; if that canonical is out of scope for the reader, the link
discloses its existence. The link is suppressed when the target is
inaccessible.

### 1.5 Catch-all sections

`THE REST` is the only section in MAINTAINERS with a catch-all file pattern
(`F: *` and `F: */`), and Linus Torvalds is its sole listed maintainer. A bug
attributed to `BTRFS FILE SYSTEM` does not carry `THE REST` in its subsystem
list, so plain set intersection would leave the top-level maintainer with no
access to most of the tree, which is the opposite of reality.

The index therefore marks a section as catch-all when any of its raw `F:`
patterns is exactly `*` or `*/`, and any maintainer of a catch-all section is
resolved with `global_maintainer = true`. Detecting this structurally rather
than hardcoding the section title means the rule keeps working if the file is
reorganized upstream.

### 1.6 Configuration

`AclSettings` gains two lists and loses one. It carries
`#[serde(deny_unknown_fields)]`, so both adding and removing keys is a hard
parse error against a mismatched config; this is why the whole branch must
merge and deploy atomically.

```toml
[server.acl]
# Sashiko operators. Full access to everything, bugs included.
admins = []

# Kernel security list. Read and comment on every bug, and read raw AI
# transcripts. Deliberately not admins: this grants no ingest, cancel or
# review capability, and no authority to close, dismiss or re-run analysis
# on a bug outside the member's own maintained subsystems.
security = []

# Principals permitted to file new bugs via /api/bug/analyze. Ships empty,
# which makes that endpoint operator-only. See 1.6a.
bug_reporters = []

ingest = []
cancel = []
review = []
blocklist = []
```

`action` is gone. Its only consumer was `/api/bug/action`, whose authorization
is now resolved per bug from `BugPrincipal`, and `get_config`'s `can_action`
hint, which must become per-bug for the same reason. A configuration key that
looks live but gates nothing is a trap, so the key and the `Permission::Action`
variant are both deleted.

### 1.6a Tool identity, and why there is no token table

`bug_reporters` ships empty. Nothing in the tree calls `/api/bug/analyze` — it
is registered but has no in-tree caller — and every bug that exists today was
created in process by the workflow, which never crosses HTTP and is therefore
unaffected by any of this. So an empty list breaks nothing and makes the
endpoint operator-only until there is an actual tool to admit.

There will be more tools: filing bugs, attaching reproducers, proposing fixes,
leaving comments. The constraint is that admitting them must not require a
schema change. Two properties deliver that:

1. **Identity stays in configuration.** A future tool is an entry in a
   capability list in `Settings.toml`, resolved into a `BugPrincipal` exactly
   like a human. This is why the design does **not** introduce the `api_tokens`
   table sketched in `DESIGN_ROLE_BASED_ACCESS_CONTROL.md`: a revocable
   per-token store is the one shape that would force a migration later.
2. **Contributions stay in `linux_bug_enrichments`.** That table already has
   `kind`, `tool`, `author`, `content` and `data_json`. A reproducer or a
   proposed fix is a new `kind` value and nothing else. No column, no table,
   no migration.

Widening tool authority later therefore means adding a capability list and a
branch in principal resolution, both of which are ordinary code changes.

### 1.6b Two addresses, and one resolved scope question

MAINTAINERS attributes two addresses to Guenter Roeck: `linux@roeck-us.net`
across four sections, and `groeck@chromium.org` across two ChromeOS EC
sections. Only the first is on the security list. Because there is no alias
map, signing in with the ChromeOS address yields a principal with management
authority over just those two ChromeOS sections and no security-list access;
signing in with the kernel.org address yields security-list access plus
management over the other four. This is a deliberate consequence of keying
authority on the address as written in MAINTAINERS.

Greg Kroah-Hartman is on the security list rather than in `admins`. This was
raised explicitly because an earlier instruction asked for him to have full
access to everything, on the understanding that he is one of the `THE REST`
maintainers; MAINTAINERS lists only Linus Torvalds there. **Decided: security
list.** He therefore reads and comments on every bug and reads every
transcript, and holds full management authority in his own eighteen
subsystems, without gaining ingest, cancel or review.

### 1.7 Building the reverse index

`MaintainersIndex` currently stores `M:` and `R:` lines raw and merged, for
example `Chris Mason <clm@fb.com>`, and never extracts an address. The `L:`
normalizer is not reusable because it takes the first whitespace token, which
on an `M:` line is the display name.

Two additions:

1. An address extractor that pulls the contents of the angle brackets, falling
   back to the whole trimmed value when there are none, then lowercases it. It
   applies to `M:` and `R:` alike; reviewers get the same authority as
   maintainers, so the parser does not need to distinguish them.
2. A `HashMap<Email, HashSet<SectionTitle>>` built once when the index is
   built, plus a `catch_all_maintainers: HashSet<Email>` set.

Both live on `MaintainersIndex` and are reachable through the existing global
initialized at startup, so resolution costs one hash lookup per request.

Note that sections with only `M:` lines and no `F:`, `N:`, `L:` or `T:` line
are dropped by the existing parser. Exactly three sections are affected. Since
a bug can only ever be attributed to a section that has file patterns, those
sections can never appear on a bug, so the parser is left alone.

### 1.8 Threats this does not address

The model is only as good as the MAINTAINERS file and the deployment. Stated
plainly:

* Anyone who can land a patch to MAINTAINERS upstream can grant themselves
  subsystem authority on the next tree sync. This is the same trust boundary
  the kernel already operates under, but it is a real one.
* Authority is keyed on an email address with no proof of control beyond
  receiving one message at it. Mail interception equals subsystem authority.
* The local operator token means anyone who can read the server's files — the
  user it runs as, and root — has operator authority over non-bug routes.

---

## Part 2: Making Subsystem Attribution Trustworthy

### 2.1 The vocabulary problem

`linux_bug_subsystems.subsystem` is free-form `TEXT` with no constraint, and
three unrelated producers write to it:

| Producer | Vocabulary | Example |
| :--- | :--- | :--- |
| MAINTAINERS matching in the bug workflow | Section titles | `BTRFS FILE SYSTEM` |
| Path-based fallback in the bug workflow | Directory prefixes, or the literal sentinel `kernel` | `fs/btrfs` |
| Caller-supplied on `/api/bug/analyze` | Whatever the caller sent, stored verbatim | anything |

Authorization keyed on section titles is only meaningful for the first. The
third is the dangerous one: a caller who can name a subsystem can currently
choose who is allowed to see the bug they file.

### 2.2 Recorded provenance

`linux_bug_subsystems` gains a provenance column, backed by a typed enum rather
than a bare string:

```sql
CREATE TABLE IF NOT EXISTS linux_bug_subsystems (
    bug_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    source TEXT NOT NULL CHECK (
        source IN ('maintainers_section', 'path_prefix', 'caller_supplied')
    ),
    PRIMARY KEY (bug_id, subsystem),
    FOREIGN KEY(bug_id) REFERENCES bugs(id) ON DELETE CASCADE
);
```

**Only rows with `source = 'maintainers_section'` are considered when resolving
maintainer access.** Path prefixes and caller-supplied strings remain useful for
display and filtering, and remain visible to operators and the security list,
but they confer no authority on anyone.

Caller-supplied `source_files` on `/api/bug/analyze` already run through
`MaintainersIndex::match_files`, so the subsystems that path derives are
genuine section titles and are recorded as such. It is only a directly supplied
subsystem *name* that is recorded as `caller_supplied`.

The column is added in the single collapsed schema migration, not as a
follow-up alter, since the branch releases atomically.

### 2.3 Hydrating subsystems on read

`parse_bug_row_core` unconditionally sets `subsystems: Vec::new()`, so any
authorization code holding a `Bug` sees an empty subsystem list unless it
explicitly hydrates. That is a trap: forgetting to hydrate silently denies
maintainers rather than failing loudly, and a future refactor could just as
easily invert it.

Authorization therefore never reads `Bug::subsystems`. It calls a dedicated
accessor that returns only the authority-bearing titles:

```rust
/// Returns the MAINTAINERS section titles attributed to this bug. Rows whose
/// provenance is a path prefix or a caller-supplied string are excluded, since
/// they confer no authority.
pub async fn authorizing_sections_for_bug(&self, bug_id: i64)
    -> Result<Vec<SectionTitle>>;
```

### 2.4 Filtering the bug list

`list_bugs` cannot filter in Rust after the fact without breaking pagination
counts. The principal's scope is pushed into SQL:

* `operator` or `global_maintainer`: no additional predicate.
* Otherwise, for a principal with maintained sections `S` and, if on the
  security list, unrestricted read:
  * security list: no additional predicate, since they read everything.
  * plain maintainer: `AND EXISTS (SELECT 1 FROM linux_bug_subsystems s
    WHERE s.bug_id = b.id AND s.source = 'maintainers_section'
    AND s.subsystem IN (...))`.
* A principal with an empty scope and no global role never reaches the query;
  the handler returns an empty page.

The existing `idx_linux_bug_subsystems_subsystem` index on
`(subsystem, bug_id)` serves this predicate. The `source` filter is evaluated
after the index seek; with at most a handful of rows per bug this is not worth
a wider index.

---

## Part 3: Enforcement

### 3.1 A resolved principal, not an ad-hoc check

Every check today is inline in a handler; there is no middleware. Rather than
adding a middleware layer that would have to re-derive which resource is being
touched, bug routes take a fallible extractor:

```rust
/// Extractor. Rejects with 401 when no valid session token is present, then
/// resolves configuration and MAINTAINERS authority into a BugPrincipal.
impl FromRequestParts<Arc<AppState>> for BugPrincipal { ... }
```

Because it is fallible, a bug route that forgets the check does not compile
into an open route: the handler has no way to reach bug data without naming the
extractor in its signature. This is the type-driven property that matters here.

`OptionalAuthUser`, which swallows every failure into `None`, is not used on
any bug route.

### 3.2 The route table after the change

| Route | Before | After |
| :--- | :--- | :--- |
| `GET /api/bugs` | none | `BugPrincipal`, list filtered to scope |
| `GET /api/bug` | none | `BugPrincipal`, `>= Read` |
| `GET /api/bug/logs` | none | `BugPrincipal`, `>= Read` **and** global bug visibility |
| `GET /api/bug/raw` | none | `BugPrincipal`, `>= Read` **and** global bug visibility |
| `GET /api/bug/input` | none | `BugPrincipal`, `>= Read` **and** global bug visibility |
| `GET /api/bug/enrichments` | none | `BugPrincipal`, `>= Read` |
| `GET /api/bugs/subsystems` | none | `BugPrincipal`, counts filtered to scope |
| `GET /api/subsystems` | none | `BugPrincipal`, counts filtered to scope |
| `GET /bug/{bugid}` | none | unchanged; it only redirects to the single-page app and answers identically for a bug that does not exist |
| `POST /api/bug/action` | global `Action` | `BugPrincipal`, per-action level |
| `POST /api/bug/analyze` | global `Review` | `BugPrincipal`, `may_create` |

`/api/bug/action` maps each variant to a required level:

| `BugAction` | Required |
| :--- | :--- |
| `Comment` | `>= Comment` |
| `Close`, `Dismiss`, `MarkDuplicate`, `Assign` | `Manage` |

The check moves *after* the bug is loaded, because it cannot be evaluated
without knowing the bug's subsystems. Actor attribution via `with_bug_actor`
stays where it is, after the check.

For `MarkDuplicate` the principal must have `Manage` on **both** the bug and
the target it is being merged into, otherwise marking a bug duplicate of an
invisible one leaks the target's existence.

### 3.3 Denial semantics

* No token, or an invalid token: `401`.
* Authenticated but `BugAccess::None` on a read: `404`, identical in body and
  timing to a genuinely absent bug. A `403` would confirm the bug exists.
* Authenticated with `Read` attempting to comment, or `Comment` attempting to
  manage: `403`, with a message naming the required authority. Here the
  existence of the bug is already known to the caller, so there is nothing to
  protect.
* Error bodies carry no backend detail. The existing leak of raw error strings
  at the analyze endpoint is fixed as part of this.

### 3.4 Bug routes are excluded from the operator bypass

`is_authorized` grants access to a request that carries no session but presents
the local operator token, a file only the user the server runs as can read.
That is deliberate for local operation and it stays, but it must not apply to
bugs: reading a file off the server's disk says nothing about which subsystems
a person maintains, which is the only question a bug route asks.

The bypass is scoped by making it a property of the *capability* rather than of
the request: `Permission::granted_by_local_token` answers yes for `Ingest`,
`Cancel` and `Review`; the new bug capabilities are not reachable that way, and
`BugPrincipal` never consults `is_authorized` at all. The predicate matches
exhaustively, so a capability added later is unreachable by the token until
someone decides it should be.

Verified impact: neither `sashiko-cli` nor `benchmark` calls any bug route, and
`local_review` drops pre-existing bugs rather than submitting them, so nothing
in the local workflow breaks. A local operator who wants to exercise bug routes
sets `testing_mode`, or configures a JWT secret and logs in.

### 3.5 Blocklist ordering

Today `is_authorized` checks `testing_mode` and `allow_all_submit` before the
blocklist, so either flag overrides a denial. The blocklist moves to the top:
a blocklisted address is denied unconditionally, before any bypass. Comparison
gains a `trim()` alongside the existing ASCII-case-insensitive match, on both
the configured entries and the incoming address.

This does not, and cannot, stop a blocklisted person who drops their session and
presents the local token instead; with no address presented there is nothing to
match. That residual case is the token's documented trust assumption — whoever
can read the server's files is already trusted with the machine — and it does
not reach bug routes.

The blocklist is also re-evaluated on session refresh, which it already is, and
on every request, which follows from resolving the principal per request rather
than baking roles into the token.

---

## Part 4: Login Hardening

Everything above rests on the session token actually meaning something. It
currently does not.

### 4.1 Sign-in links are delivered by email

The link is presently written to the server log and never sent. It must be
mailed, and mail must go through the existing outbox rather than a new
synchronous path, so that it inherits retry, dry-run and transport handling.

Three obstacles in `email_outbox`:

* `patch_id` is `NOT NULL`. It becomes nullable, following the precedent
  already set by `insert_patchwork_notification`.
* The dedup guard keys on `patch_id` alone, so a second login mail would be
  suppressed. Dedup becomes conditional on there being a `patch_id`.
* `dry_run` suppresses at two independent layers: the worker early-returns, and
  producers write a `Dry-Run` status that the poller never picks up. Login mail
  must replicate the status behavior, not just inherit the worker check, or
  rows will pile up unsent in a dry-run deployment.

An explicit `kind` column distinguishes review notifications from transactional
mail, so the two can be observed and rate-limited separately.

When SMTP is unconfigured the worker is never spawned. In that case the
endpoint still returns its generic success response and the link is logged, as
today, so local operation is unaffected. Only in that configuration is a link
ever logged.

### 4.2 The message

Single-part `text/plain`, matching every other message Sashiko sends. No HTML,
no tracking, no images. Subject tagged in the house style.

```
From: Sashiko <sashiko@example.org>
To: maintainer@example.org
Subject: [sashiko] Your sign-in link
List-Id: <sashiko-auth.example.org>
Auto-Submitted: auto-generated

Someone asked to sign in to Sashiko as maintainer@example.org.

Open this link within 30 minutes to continue:

  https://sashiko.example.org/auth/verify?token=<jwt>

If you did not ask to sign in, ignore this message. Nothing has changed and
nobody has gained access.

-- 
Sashiko AI review · https://sashiko.example.org
```

`Auto-Submitted: auto-generated` suppresses vacation autoresponders.
`List-Id` lets recipients file it. The signature reuses the existing
delimiter and separator exactly.

### 4.3 A public base URL

The link is currently built from `server.host` and `server.port` with a
hardcoded `http://`, which with the shipped `host = "::"` renders the unusable
`http://:::8080/auth/verify?token=...`.

A `server.public_base_url` setting is added, and **the server refuses to start
when SMTP is configured and this is unset or names a wildcard bind address.**
Failing at deploy is far better than failing at three in the morning when a
maintainer cannot log in, and it is the only failure mode an operator can act
on immediately.

When SMTP is not configured the setting is optional, because the link is
logged rather than mailed and a local operator can read the log. In that case
the server derives a best-effort URL as today.

### 4.4 Stateless sign-in tokens

Authorization remains completely stateless. The sign-in link carries an
HMAC-SHA256 signed JWT with `typ: "sign_in_link"` and a 30-minute expiration
(`1800` seconds). No database table is required to store or track issued links.

The session token extractor strictly enforces token types (`typ: "session"`),
preventing a sign-in link token from being used directly as a session bearer
token on API routes.

### 4.5 The request endpoint reveals nothing

`/api/auth/request-link` currently answers `403` for an unknown or blocklisted
address and `200` for a known one, which turns it into an oracle for
discovering exactly which addresses are on the security list.

It becomes unconditional: always `200`, always the same body, for every
syntactically plausible address. Mail is sent only if the address is eligible.
The two paths are equalized in observable cost, so an ineligible request cannot
be distinguished by timing either. A malformed address gets the same response;
there is no validation feedback.

### 4.6 Rate limits

Chosen values, keyed independently and all fail-closed on overflow:

| Key | Limit | Rationale |
| :--- | :--- | :--- |
| Per address | 3 per 15 minutes, 10 per day | Caps use as a mail bomb against one person |
| Per source address | 10 per 15 minutes | Caps enumeration sweeps from one client |
| Global | 100 per hour | Caps total outbound login mail regardless of distribution |

Enforcement is in-process, which is sufficient for a single-replica deployment
with `strategy: Recreate`. A rate-limited request returns the same `200` and
the same body as any other; only the mail is withheld.

### 4.7 Session tokens

* `alg` is pinned. Validation constructs `Validation::new(Algorithm::HS256)`
  explicitly and rejects any other algorithm, closing the `alg` confusion class
  even though the current key type makes it hard to exploit.
* `typ` becomes an allowlist over a typed enum. Today it is a denylist that
  rejects only `sign_in_link`, so a token with no `typ` at all is accepted. After
  the change, anything that is not exactly a session token is rejected.
* Claims gain `iat` and `sid`, a session identifier fixed at first issue and
  carried across refreshes.
* An absolute session lifetime of 30 days is enforced against `iat`-of-session,
  so refresh can extend a session but cannot extend it forever. Refresh
  continues to re-check the blocklist.
* The JWT secret has no default and no generated fallback. The three auth
  handlers gain the same `JWT_SECRET` environment fallback the extractor
  already has, and a server started without a secret refuses to serve auth
  routes rather than silently failing per-request. `jwt_secret` is documented
  in `Settings.toml` as required for any deployment exposing bugs.

---

## Part 5: Frontend

The bug list and bug page must handle `401` and `404` distinctly:

* `401` clears the stored session and shows the login prompt.
* `404` on a bug the user navigated to directly shows "not found", with no hint
  that it might exist but be inaccessible.
* Action buttons are rendered from the access level the API reports for that
  bug, so a commenter does not see a close button that would fail. The server
  check remains authoritative; this is presentation only.

The API returns the caller's level alongside each bug for exactly this purpose,
which is cheap since it is already computed to answer the request.

---

## Implementation Order

Each step is one commit, self-contained and independently buildable.

**Schema**

1. Collapse the linux bug schema into a single migration.
2. Record the provenance of bug subsystems.

**Identity**

3. Extract addresses from maintainer entries.
4. Index subsystems by maintainer address.
5. Add security and bug reporter capability lists.
6. Resolve per-principal bug authority.

**Enforcement**

7. Restrict bug reads to authorized principals.
8. Withhold raw analysis transcripts from scoped maintainers.
9. Scope bug mutations to maintainer subsystems.
10. Exclude bug endpoints from the operator bypass.
11. Require the create capability to submit bugs.
12. Evaluate the blocklist ahead of every bypass.
13. Drop the unused global action capability.

**Login**

14. Generalize the email outbox for transactional mail.
15. Add the public base URL option.
16. Deliver sign-in links by email.
17. Answer link requests without revealing eligibility.
18. Rate limit sign-in link requests.
19. Pin the token algorithm and bound session lifetime.

**Frontend**

20. Handle unauthorized bug access.

**Referential integrity**

Not part of the access model, but this branch is what turns these from silent
corruption into hard failures, so they ship with it.

21. Merge patchsets in a single transaction.

---

## Testing

* `BugPrincipal::access_to` is table-driven over the full cross product of
  operator, security, global maintainer, scoped maintainer and unprivileged,
  against bugs with a matching subsystem, a non-matching subsystem, several
  subsystems, a path-prefix-only attribution, the `kernel` sentinel, and no
  subsystems at all.
* Address extraction is tested against real MAINTAINERS forms: angle brackets,
  quoted display names containing commas, a bare address with no display name,
  and mixed case.
* Catch-all detection asserts that `THE REST` is detected and that no other
  section in the checked-in MAINTAINERS file is.
* Every bug route has a test asserting `401` without a token and `404` with a
  token that lacks access, including the raw log and raw input routes, which
  are the highest-value targets.
* `MarkDuplicate` is tested for the case where the caller can manage the source
  but not the target.
* Sign-in link exchange is tested for expiry, signature verification, and type
  enforcement.
* `/api/auth/request-link` is tested to return byte-identical responses for an
  eligible address, an ineligible address, a blocklisted address and a
  malformed address.
* Session refresh is tested to fail once the absolute lifetime is exceeded and
  once the address is blocklisted.
* An email composition test asserts the login message body byte for byte,
  following the existing convention for review notification tests.

## Decisions

Resolved during review, recorded so the reasoning is not relitigated:

| Question | Decision |
| :--- | :--- |
| Raw transcripts leak other bugs through the dedup stage | Restrict `logs`, `raw` and `input` to principals with global bug visibility (1.4a) |
| Greg Kroah-Hartman: security list or `admins`? | Security list (1.6b) |
| May the security list trigger re-analysis? | No. It is a spend-money action and belongs to `Manage` |
| Absolute session lifetime | 30 days |
| Tool identity | Config-driven only. No `api_tokens` table, so future tools need no migration (1.6a) |
| Dead `Permission::Action` | Deleted along with the `action` config key |
| Missing `public_base_url` with SMTP configured | Refuse to start |
| The patchset merge defect | Fixed on this branch, as its own commit |

## Open Questions

1. The 1141 pre-existing `tool_usages` rows orphaned by seven reviews deleted
   on 8–9 May 2026. They are unreachable by every query in the codebase, but
   they keep invariant 1 of `check_invariants.sh` permanently red, which
   blinds it to genuinely new orphans. Sweep them in the migration, or leave
   them for a deliberate operator step so the May incident stays forensically
   intact?
