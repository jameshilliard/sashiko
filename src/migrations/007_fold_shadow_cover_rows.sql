-- Folds empty cover-letter shadow rows into the Reviewed series they shadow.
--
-- When a cover letter (0/N) arrived after patch 1/N had already created a
-- patchset row, create_patchset updated the patch row's subject and
-- subject_index to 0, and then a second pass saw subject_index = 0 with a
-- different cover_letter_message_id, flagged index_collision, and minted an
-- empty Incomplete row holding the cover letter's message id.
--
-- The result is two rows in the same thread with the same subject:
--   * an empty Incomplete row holding the cover letter's message id
--   * a Reviewed row holding all the patches and reviews under patch 1's id
--
-- Anyone opening the cover letter's URL lands on the empty Incomplete page.
--
-- To guarantee zero side effects (no new reviews started, no emails queued or
-- re-sent), we only fold pairs where:
--   1. The empty row has status = 'Incomplete', 0 patches, and 0 reviews.
--   2. No other row claims the empty row's cover_letter_message_id.
--   3. The real row has status = 'Reviewed' and embargo_until IS NULL.
--   4. Every patch of the real row already has an email_outbox entry, so
--      insert_email_outbox()'s duplicate guard would skip sending even if
--      release were ever re-invoked.
--   5. Exactly one such empty row and one such real row exist for that
--      (thread_id, subject) pair.

CREATE INDEX IF NOT EXISTS idx_patchsets_thread_id ON patchsets(thread_id);

-- The pairing below is worked out fresh on every run. A temp table left over
-- from an earlier run on this connection would otherwise survive, and
-- CREATE TEMP TABLE IF NOT EXISTS would keep its rows instead of the ones the
-- SELECT would find now.
DROP TABLE IF EXISTS temp._shadow_cover_fold;

CREATE TEMP TABLE _shadow_cover_fold AS
WITH candidates AS (
    SELECT e.id AS empty_id,
           e.cover_letter_message_id AS honest_name,
           (
               SELECT r.id
                 FROM patchsets r
                WHERE r.id != e.id
                  AND r.thread_id = e.thread_id
                  AND r.subject = e.subject
                  AND r.status = 'Reviewed'
                  AND r.embargo_until IS NULL
                  AND EXISTS (
                          SELECT 1
                            FROM patches p
                           WHERE p.patchset_id = r.id
                             AND p.message_id = r.cover_letter_message_id
                      )
                  AND NOT EXISTS (
                          SELECT 1
                            FROM patches p
                           WHERE p.patchset_id = r.id
                             AND NOT EXISTS (
                                     SELECT 1
                                       FROM email_outbox eo
                                      WHERE eo.patch_id = p.id
                                 )
                      )
                  AND NOT EXISTS (
                          SELECT 1
                            FROM patchsets r2
                           WHERE r2.id != r.id
                             AND r2.id != e.id
                             AND r2.thread_id = e.thread_id
                             AND r2.subject = e.subject
                             AND r2.status = 'Reviewed'
                      )
                ORDER BY r.id ASC
                LIMIT 1
           ) AS real_id
      FROM patchsets e
     WHERE e.status = 'Incomplete'
       AND e.cover_letter_message_id IS NOT NULL
       AND e.cover_letter_message_id NOT LIKE '%@sashiko.local'
       AND NOT EXISTS (
               SELECT 1 FROM patches x WHERE x.patchset_id = e.id
           )
       AND NOT EXISTS (
               SELECT 1 FROM reviews rv WHERE rv.patchset_id = e.id
           )
       AND NOT EXISTS (
               SELECT 1
                 FROM patchsets o
                WHERE o.id != e.id
                  AND o.cover_letter_message_id = e.cover_letter_message_id
           )
)
SELECT empty_id, real_id, honest_name
  FROM candidates
 WHERE real_id IS NOT NULL
 GROUP BY real_id
HAVING count(*) = 1;

INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id)
SELECT f.real_id, ps.subsystem_id
  FROM _shadow_cover_fold f
  JOIN patchsets_subsystems ps ON ps.patchset_id = f.empty_id;

DELETE FROM patchsets_subsystems
 WHERE patchset_id IN (SELECT empty_id FROM _shadow_cover_fold);

UPDATE bugs
   SET discovered_in_patchset_id = (
           SELECT f.real_id
             FROM _shadow_cover_fold f
            WHERE f.empty_id = bugs.discovered_in_patchset_id
       )
 WHERE discovered_in_patchset_id IN (SELECT empty_id FROM _shadow_cover_fold);

UPDATE patchsets
   SET cover_letter_message_id = (
           SELECT f.honest_name
             FROM _shadow_cover_fold f
            WHERE f.real_id = patchsets.id
       )
 WHERE id IN (SELECT real_id FROM _shadow_cover_fold);

DELETE FROM patchsets
 WHERE id IN (SELECT empty_id FROM _shadow_cover_fold);

DROP TABLE _shadow_cover_fold;
