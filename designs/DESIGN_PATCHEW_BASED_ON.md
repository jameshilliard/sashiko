# Patchew Based-on Support

## Objective

Apply a pending prerequisite series named by Patchew's `Based-on` metadata
before reviewing the dependent patchset. This extends the prerequisite
support from exact b4 patch IDs to a message ID that names an entire series.

The implementation supports metadata present in the cover letter or in the
single patch body. It implements the `Based-on` convention without depending
on the Patchew service or its database.

## Metadata

Recognize an unquoted line at the beginning of a body line with this form:

```text
Based-on: prerequisite-series-message-id@example.com
```

Optional angle brackets around the message ID are accepted. Tag names are
matched case-insensitively, duplicate values are ignored, and more than one
distinct `Based-on` value is rejected because Patchew does not define an
ordering for multiple base series. Empty values, comments on the same line,
non-ASCII or whitespace characters inside the message ID, values longer than
998 bytes, and unmatched angle brackets are rejected.

Quoted or indented lines are not active metadata. This avoids treating a
quoted discussion, prose example, or diff context as a dependency.

## Resolution

The message ID must name the head of a complete series. Resolve it in this
order:

1. Find the series in Sashiko's server-side database.
2. If no complete local copy exists, fetch the message thread from Lore's
   public-inbox mbox endpoint.
3. Locate the exact named series head and select only its descendant patch
   messages.
4. Require one patch for every part number and order them by part number.

A local hit does not depend on the prerequisite patchset reaching `Reviewed`.
Stored patch content is sufficient. Nullable stable patch IDs from databases
created before the b4 migration are calculated while resolving the series.

Lore threads may include reviews, prior discussion, or another patch series.
The resolver must never treat every patch in the returned thread as part of
the prerequisite. A named cover selects matching descendant patches. A
coverless series may be named by its first patch and selects matching patch
descendants. A later patch in a series is not a valid series head.

## Nested and Mixed Dependencies

A prerequisite series may contain its own `Based-on` metadata. Resolve these
dependencies recursively and apply the deepest series first. Detect cycles by
normalized message ID, including a dependency back to the target series, and
reject chains deeper than eight series.

For each series, apply its nested `Based-on` chain, then its b4
`prerequisite-patch-id` entries in declared order, then the series itself. For
the target patchset, apply the expanded `Based-on` chain before its b4
prerequisite patches. Deduplicate the resulting patch list by stable Git patch
ID while preserving the first occurrence.

This ordering makes `Based-on` the base of the dependent series and remains
safe when both formats describe the same prerequisite patches.

## Resource and Trust Boundaries

Keep the existing limit of 128 unique prerequisite patches. Share the limit of
eight remote Lore operations across message-ID thread fetches and patch-ID
searches in one resolution. Reuse the existing HTTP timeout, retry, compressed
download, decompressed mbox, message-count, and Git patch-ID subprocess limits.

Treat message IDs and downloaded email as untrusted input. Encode a message ID
as one URL path segment without allowing it to change the configured Lore
origin. Reject malformed series, duplicate part numbers, missing parts,
non-head references, dependency cycles, and limits exceeded. Any such failure
stops baseline preparation rather than reviewing against a different tree.

## Worktree Behavior

Flatten resolved dependencies into the existing `PrerequisitePatch` type and
use the existing worktree application path. Prerequisites remain preparation
context only:

- They are applied before the target patchset for every baseline candidate.
- Their final commit is the effective review baseline.
- They are excluded from the target review payload.
- They do not change the target patch-to-commit mapping.
- Lore-only fetches are not persisted or scheduled for separate review.

No schema migration is required.

## Testing

- Parser tests cover angle brackets, case, duplicate tags, ambiguous tags,
  malformed values, quoted lines, and b4 metadata coexistence.
- Series extraction tests cover cover letters, coverless single patches,
  unrelated patches in the same Lore thread, missing and duplicate parts, and
  non-head message IDs.
- Resolver tests cover local lookup, Lore fallback, nested dependencies,
  stable patch-ID deduplication, cycles, depth, and shared remote-operation
  limits.
- Reviewer tests verify that a target patch which fails on the upstream base
  applies after its `Based-on` series and still uses the prerequisite tip as
  its effective review baseline.
- Existing b4 prerequisite tests continue to protect patch-ID-only behavior.

## Out of Scope

- Reading `Based-on` tags from replies posted after the cover or patch.
- Requeueing a patchset when a later reply adds or changes dependency tags.
- Querying Patchew's API or consuming Patchew-hosted Git tags.
- Supporting multiple distinct `Based-on` values on one series.
- Persisting Lore-only prerequisite series as ordinary patchsets.
