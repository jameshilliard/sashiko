# Design: Patchset Subsystem Detection and Maintainer-Scoped Raw Logs

## 1. Problem

A Linux kernel bug records which MAINTAINERS sections it belongs to, and that
record decides who may read it. A patchset records nothing of the kind, and its
raw LLM transcript is served to anybody who asks.

The transcript is the whole conversation the reviewer had with the model: every
prompt, every tool call, every file the model read out of the kernel tree, and
every intermediate conclusion it discarded. Today `reviews.logs` reaches an
unauthenticated caller through three separate paths:

| Path | Handler | How the transcript leaks |
| --- | --- | --- |
| `GET /api/review_log` | `get_review_log` | Whole review row, `logs` included |
| `GET /api/review` | `get_review` | Whole review row, `logs` included |
| `GET /api/patch` | `get_patchset` | Every review embedded in the patchset carries its own `logs` |

The third is the one that matters most: it is the ordinary patchset page, so the
transcript ships on every page load whether or not anyone asked for it.

This design gives a patchset the same subsystem attribution a bug has, and uses
it to decide who may read that patchset's transcripts.

## 2. What already exists

### 2.1 The bug model, which this follows

`bug_subsystems(bug_id, subsystem, source)` records a name and where it came
from. `source` is one of `maintainers_section`, `path_prefix` or
`caller_supplied`, and only the first confers authority, because only a
MAINTAINERS section title names people the kernel trusts. A directory prefix
names nobody, and a name the caller invented would otherwise let the caller
choose their own audience.

`BugPrincipal` resolves one caller's authority once per request from the ACL
lists and the `MaintainersIndex`, and `access_to(&[SectionTitle])` composes the
grants by taking the strongest.

Raw bug transcripts are narrower still: they need `has_global_bug_visibility()`,
which excludes subsystem maintainers. That restriction is specific to bugs. The
deduplication stage compares a candidate against every other bug in the
database and the prompt embeds each candidate's problem statement, so a bug
transcript discloses unrelated bugs by construction.

### 2.2 Why patch transcripts are different

A patch review transcript is the history of one series being reviewed against
one baseline. It contains that series, whatever the model read out of the kernel
tree, and the model's reasoning. It does not enumerate other patchsets. There is
no cross-series stage equivalent to bug deduplication.

So a patch transcript can safely go to the maintainers of the sections the
series touches, which is what makes "corresponding maintainers" the right
audience here where it would be the wrong audience for a bug.

### 2.3 The existing patchset subsystem tables are not this

`patchsets_subsystems`, `patches_subsystems` and `messages_subsystems` join to a
`subsystems(name, mailing_list_address)` table populated by
`identify_subsystems` from the `To:`/`Cc:` headers and by
`identify_subsystems_from_paths` from configured regexes. Those rows hold
mailing list labels such as `netdev` or `LKML`, keyed by list address.

A mailing list label cannot confer authority. Anybody can put an address in
`Cc:`, and the label names a list rather than a person. These tables are left
exactly as they are and are not consulted by access control.

The new table is therefore named `patchset_maintainer_sections` rather than
`patchset_subsystems`. The name states what the rows are, MAINTAINERS section
titles, and it cannot be misread as the mailing list table by someone scanning
a query.

> [!NOTE]
> Naming the new table `patchset_subsystems` and renaming the legacy one out of
> the way was tried and abandoned. Migration 007 names `patchsets_subsystems` in
> its own SQL, and a shipped migration has to keep naming the schema as it stood
> when it was written. Renaming the table in a later migration means 007 can
> never be replayed against a database that has reached the later version, which
> broke two migration tests and would leave a restored or half-applied database
> unrecoverable. Avoiding the collision by naming the new table well costs
> nothing and carries no such hazard.

## 3. Model

### 3.1 Attribution

A patchset is attributed to the union of the MAINTAINERS sections matched by the
files its patches touch:

```
Sections(P) = ⋃ over patches p in P, over files f in diff(p) : MatchSection(f)
```

Sections are stored with their provenance, in a table that mirrors
`bug_subsystems` exactly:

```sql
CREATE TABLE IF NOT EXISTS patchset_maintainer_sections (
    patchset_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'caller_supplied'
        CHECK (source IN ('maintainers_section', 'path_prefix', 'caller_supplied')),
    PRIMARY KEY (patchset_id, subsystem),
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_patchset_maintainer_sections_subsystem
    ON patchset_maintainer_sections(subsystem, patchset_id);
CREATE INDEX IF NOT EXISTS idx_patchset_maintainer_sections_authorizing
    ON patchset_maintainer_sections(patchset_id, subsystem)
    WHERE source = 'maintainers_section';
```

The `source` column is retained even though the writer only ever produces
`maintainers_section` today. Keeping the column means the existing
`SubsystemSource` type, its fail-closed `from_stored`, and its
`confers_authority` predicate are reused verbatim rather than reimplemented, and
a later writer that wants to record a path prefix for display cannot
accidentally grant authority with it.

Path prefixes are deliberately **not** written. For a bug they are a display
fallback; here they would be noise that confers nothing, and the empty case
already has a defined meaning (section 3.3).

### 3.2 Who may read a patchset's transcripts

Granted to any caller who satisfies at least one of:

1. Presents this process's local operator token.
2. Is an operator (`acl.admins`).
3. Is on the kernel security list (`acl.security`).
4. Is the global maintainer, that is, maintains a section claiming the whole
   tree (`THE REST`).
5. Maintains or reviews at least one section attributed to the patchset with
   `source = 'maintainers_section'`.
6. Is the author of the series.

Denied otherwise. A blocklisted address resolves to the anonymous principal
before any of this runs, so it satisfies none of them.

Grant 6 needs care. `patchsets.author` is a display string such as
`Jane Doe <jane@example.org>`; the comparison parses the address out of it with
the existing `maintainers::maintainer_address` helper and compares case
insensitively against the address the session token proved. An author string
that yields no parsable address grants nobody.

> [!IMPORTANT]
> Grant 1 is a deliberate divergence from `BugPrincipal`, which never consults
> the local token. It is required here because `sashiko-cli` fetches
> `/api/review_log?patchset_id=...` with the local token and no identity, and
> because a patch transcript is not a kernel bug report. The divergence is
> confined to the extractor and is not reachable from any bug route.

### 3.3 When nothing matched

A patchset with no `maintainers_section` rows fails closed: grants 5 is
unavailable, and only the operator, security, global maintainer and author
grants remain.

This covers forge and merge-request submissions against non-Linux projects,
series whose diffs never parsed, and cover-letter-only sets. Those transcripts
become invisible to the public where they are visible today. That is the
intended direction: the alternative, leaving unattributed transcripts public,
would make "touch a file nobody maintains" a way to publish a transcript.

### 3.4 What stays public

Only the transcript is restricted. Findings, severities, the inline review, the
summary, the patchset metadata and the diffs are unchanged and remain readable
without a session. Nothing that is public today becomes private except
`reviews.logs`.

`patchsets.baseline_logs` is out of scope: it is `git am` and baseline detection
output about the series itself, not a model transcript.

## 4. Code shape

### 4.1 The principal is no longer bug specific

`BugPrincipal` already carries everything grants 2 through 5 need: `operator`,
`security`, `global_maintainer`, `maintained_sections` and `email`. Only the
composition differs between domains.

`src/bug_access.rs` is renamed to `src/access.rs` and `BugPrincipal` to
`Principal`. `BugAccess`, `SectionTitle` and the extractors keep their names.
This is a mechanical rename with no behaviour change, landed as its own commit
so that the behavioural commits are readable.

The alternative, leaving the type called `BugPrincipal` and using it to guard
patch transcripts, was rejected: a name that lies about its domain is exactly
the confusion `SectionTitle` exists to prevent.

### 4.2 The new decision type

```rust
/// Whether a caller may read a patchset's raw review transcripts.
///
/// Two states rather than a bool so that the decision cannot be confused with
/// any other predicate at a call site, and so that a future third state has
/// somewhere to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptAccess {
    Denied,
    Granted,
}

/// A principal together with the local-token evidence the HTTP edge saw.
///
/// Extracted infallibly: an anonymous caller resolves to a principal holding
/// nothing rather than a rejection, because the routes this guards also serve
/// public data and must not start refusing it.
pub struct TranscriptPrincipal {
    principal: Principal,
    local_operator: bool,
}

impl TranscriptPrincipal {
    pub fn access_to_patchset(
        &self,
        attributed: &[SectionTitle],
        author: Option<&str>,
    ) -> TranscriptAccess;
}
```

### 4.3 Database accessors

Mirroring the bug accessors, in `src/db.rs`:

- `replace_patchset_maintainer_sections(patchset_id, &[AttributedSubsystem])`
  — upsert and prune, reusing `UPSERT_BUG_SUBSYSTEM_SQL`'s shape.
- `add_patchset_maintainer_sections(patchset_id, &[AttributedSubsystem])`
  — union only, used on ingestion because a series arrives one part at a time.
- `authorizing_sections_for_patchset(patchset_id) -> Vec<String>` — the
  `source = 'maintainers_section'` rows, which is all authorization ever asks
  for.
- `patchset_maintainer_sections(patchset_id) -> Vec<AttributedSubsystem>`
  — for display.

### 4.4 Detection at ingestion

In `main.rs`, the block that already calls
`baseline::extract_files_from_diff(&p.diff)` to feed
`identify_subsystems_from_paths` gains a second consumer: the files are matched
against `maintainers::get_global_maintainers()` and the resulting section titles
are added to the patchset with `AttributedSubsystem::from_maintainers`.

Because parts arrive independently and each adds to the union, `add_` rather
than `replace_` is used here. Nothing is written when the index is absent,
which leaves the patchset unattributed and therefore closed.

The same helper is called from the `/api/submit` and forge webhook paths so that
a series that never came off NNTP is attributed identically. The helper lives in
one place; no call site reimplements the matching.

### 4.5 Backfill

Existing patchsets have no rows. MAINTAINERS matching cannot run in SQL, so the
backfill is a one-shot startup job in Rust rather than a migration: it walks
patchsets that have no `patchset_maintainer_sections` rows, reads their
patches' stored diffs, matches, and writes. Completion is recorded in the
`meta` table so the walk happens once.

Until it completes, an unattributed patchset is closed rather than open, so the
job running late is a nuisance and never a disclosure.

### 4.6 Enforcement

| Endpoint | Behaviour when denied |
| --- | --- |
| `GET /api/review_log` | `403 Forbidden` |
| `GET /api/review` | 200, with the `logs` key removed |
| `GET /api/patch` | 200, with `logs` removed from every embedded review |

`/api/review_log` refuses outright because asking it for anything is asking for
the transcript. The other two serve public data alongside the transcript, so
they answer and omit. The refusal is 403 and not 404 because the patchset's
existence is already public.

Both `/api/review` and `/api/review_log` accept `id` or `patchset_id`, so each
resolves the review's owning patchset before deciding.

`/api/patch` gains a `can_read_logs` boolean so the UI knows whether to offer
the link rather than offering one that fails.

Redaction happens in the handlers, next to the existing `redact_embedded_bugs`
call, so that every path out of `get_patchset_details` is covered by one place.

> [!IMPORTANT]
> `GET /api/patch` has shipped the full transcript inside every embedded review
> since before there was a transcript viewer, and callers depend on the rest of
> that payload. Denying a caller must remove the `logs` key and change nothing
> else: same status, same field order everywhere else, no substituted
> placeholder, no new error. A consumer that never read `logs` must not be able
> to tell the difference, and one that did must see an absent key rather than a
> failed request.

The same rule applies to `/api/review`. Only `/api/review_log`, whose entire
purpose is the transcript, answers with a status the caller has to handle, and
`sashiko-cli` is the one client that calls it: it presents the local token, so
grant 1 keeps it working unchanged.

Redaction is driven by the same `logs` key in both handlers, so there is one
list of what counts as a transcript rather than a per-endpoint list that can
drift.

## 5. UI

- `renderLogView` moves from `/api/review?id=` to `/api/review_log?id=`, and
  renders the sign-in and restricted states already written for the bug raw log
  view rather than a generic error.
- The restricted message names the audience: maintainers of the affected
  subsystems, the security team, and operators.
- The "View Raw Log" affordance is rendered only when `can_read_logs` is true.
- The patchset header renders the detected MAINTAINERS sections as tags, in the
  style bugs already use.

## 6. Commits

1. Rename `bug_access` to `access` and `BugPrincipal` to `Principal`. No
   behaviour change.
2. Add the `patchset_maintainer_sections` table, the database accessors and
   their tests.
3. Detect and record sections at ingestion, for NNTP, `/api/submit` and forge
   webhook paths.
4. Backfill existing patchsets at startup, once, recorded in `meta`.
5. Add `TranscriptAccess` and `TranscriptPrincipal`, with unit tests for the
   grant composition.
6. Enforce on `/api/review_log`, `/api/review` and `/api/patch`; publish
   `can_read_logs`.
7. Render the restricted states and the subsystem tags in the UI.

Each is self-sufficient and leaves the tree green. Commits 1 through 4 change no
access decision, so the restriction lands exactly once, in commit 6, by which
time attribution is already in place for old and new patchsets alike.

## 7. Testing

- Unit tests in `src/access.rs` covering every grant and the closed default:
  section maintainer of a touched section, maintainer of an untouched section,
  security, operator, global maintainer, author, local token, anonymous,
  blocklisted maintainer, and the unattributed patchset.
- Author matching tests: display-name forms, case differences, unparsable
  strings, and an empty author.
- Database tests for union semantics across parts, provenance round-tripping,
  and `authorizing_sections_for_patchset` ignoring non-authorizing rows.
- Integration tests for each of the three endpoints, denied and granted.
- `tests/integration/server_tests.rs::test_review_endpoint_returns_logs`
  asserts an unauthenticated caller receives `logs`. It is updated to assert the
  new behaviour and a granted counterpart is added beside it.

## 8. What this does not do

- It does not restrict findings, diffs or any other patchset data.
- It does not touch `patchsets.baseline_logs`.
- It does not change bug access in any way.
- It does not give patch subsystem maintainers any authority over bugs, nor bug
  subsystem maintainers any authority over patchsets beyond what the shared
  MAINTAINERS sections already imply.

