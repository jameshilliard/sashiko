# Bug analysis lease ownership

## Problem

Renewal detects a replacement worker, but the old analysis continues writing.
Its completion or failure can overwrite the replacement's state, and its release
can erase the replacement's lease. A process identifier also cannot distinguish
two attempts reclaimed by the same process.

## Repair

Keep the existing lease columns and retry policy. Give each attempt a fresh
identifier consisting of the process identity and a random 128-bit suffix.
Carry the bug ID and attempt identifier in the analysis's Database handle;
attribution handles retain this claim context.

Before stage enrichment, outcome, duplicate, review-link, failure, or release
writes, validate the claim inside the same write transaction. The stored owner
must match, the lease must still be valid, and the execution state must be
running or succeeded. The last state allows the successful attempt to finish
its review links and release. Atomic outcome persistence is already in place.
Analysis writes to a different source bug are rejected. Unclaimed handles keep
the existing operator and ingestion behavior.

Automatic deduplication retains its claim until the worker finishes linking and
releasing. Human deduplication still clears it, causing the running analysis to
lose ownership. A scoped failure may only fail a running attempt; it cannot turn
a committed successful result into a failed attempt.

Run renewal and analysis as sibling futures. Losing ownership drops the analysis
future. Dropping or unwinding the analysis drops renewal as well, without a
separate detached heartbeat task. Bound each renewal by the last confirmed lease
deadline; transient errors may retry inside that window but cannot extend it.
Ownership checks remain necessary because cancellation can race with completion
and cannot undo already submitted external requests.

## Validation

- Expire and reclaim a lease using two connections to one temporary database.
  The stale handle must fail every analysis write, including after changing
  attribution. It must not clear or renew the replacement's lease.
- Repeat with two attempt identifiers from the same process identity.
- Verify expired leases reject writes even before a replacement claims them.
- Verify the owner can record stages, commit outcomes, link reviews, and release;
  verify duplicate completion keeps the claim through review linking.
- Verify a failure after successful persistence cannot change the result.
- Verify loss, normal completion, and unwinding drop their sibling future.
- Run make check-pr before committing.

## Review

The repair needs no migration, new scheduler, or authentication change. Claim
metadata is internal and is not included in bug API payloads. Persisted earlier
stage logs remain available after cancellation. Cancellation bounds additional
work; the database ownership check determines whether writes may commit.
