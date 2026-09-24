-- Re-points patchsets that were named after somebody else's message at
-- their own first patch.
--
-- A part of a series used to take the message it replied to as the series'
-- name, so a series posted into another thread claimed a message that
-- belongs to the series that sent it. A lookup resolves a name to a cover
-- letter before it resolves it to a patch, so the borrower wins the URL and
-- the rightful series is unreachable under its own message id.
--
-- A row whose subject_index is above 0 never saw a cover letter of its own,
-- because a cover letter is part 0 and always lowers that index to 0. If
-- such a row is also named after a message that is none of its own patches,
-- the name is borrowed and its own first patch is the honest one.
--
-- Only a name that another patchset has a claim to is rewritten, either
-- because that patchset answers to the same name or because the name is one
-- of its patches. A name nobody else claims harms nothing, whatever its
-- provenance, and rewriting it would churn URLs that people already hold.
-- A name a fetch would come back to is left alone for the same reason: it is
-- the handle that fetch is tracked by. A fetch of a commit mints a
-- sashiko.local placeholder, but a fetch of a thread names the row after the
-- very message id it was asked for, and that name is a real one. Either kind
-- is reused, rather than duplicated, only while the row sits in a state
-- create_fetching_patchset restarts from, so those states are skipped.
--
-- A rename never lands on a name another patchset already holds or already
-- carries as a patch. Two rows that share a first patch are a separate
-- fragmentation defect, and moving both onto one name would trade the
-- collision being repaired here for a fresh one.
WITH own_first_patch AS (
    SELECT p.patchset_id AS patchset_id,
           p.message_id  AS message_id
      FROM patches p
     WHERE p.id = (
               SELECT q.id
                 FROM patches q
                WHERE q.patchset_id = p.patchset_id
                ORDER BY q.part_index ASC, q.id ASC
                LIMIT 1
           )
)
UPDATE patchsets
   SET cover_letter_message_id = own_first_patch.message_id
  FROM own_first_patch
 WHERE own_first_patch.patchset_id = patchsets.id
   AND patchsets.subject_index > 0
   AND patchsets.cover_letter_message_id IS NOT NULL
   AND patchsets.cover_letter_message_id NOT LIKE '%@sashiko.local'
   -- A fetch finds the row it already created by name, and only in these
   -- states, so renaming one of them would make the next attempt mint a
   -- second row instead of reusing this one.
   AND patchsets.status NOT IN ('Fetching', 'Failed', 'Cancelled',
                                'Failed To Apply', 'FailedToApply')
   -- The name is none of this series' own patches, so it is borrowed.
   AND NOT EXISTS (
           SELECT 1
             FROM patches p
            WHERE p.patchset_id = patchsets.id
              AND p.message_id = patchsets.cover_letter_message_id
       )
   -- Another patchset has a claim to the borrowed name.
   AND (
           EXISTS (
               SELECT 1
                 FROM patchsets other
                WHERE other.id != patchsets.id
                  AND other.cover_letter_message_id =
                      patchsets.cover_letter_message_id
           )
        OR EXISTS (
               SELECT 1
                 FROM patches p
                WHERE p.patchset_id != patchsets.id
                  AND p.message_id = patchsets.cover_letter_message_id
           )
       )
   -- Nobody else holds the honest name, so taking it collides with nothing.
   AND NOT EXISTS (
           SELECT 1
             FROM patchsets other
            WHERE other.id != patchsets.id
              AND other.cover_letter_message_id = own_first_patch.message_id
       )
   AND NOT EXISTS (
           SELECT 1
             FROM patches p
            WHERE p.patchset_id != patchsets.id
              AND p.message_id = own_first_patch.message_id
       );
