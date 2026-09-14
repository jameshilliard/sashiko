-- Distinguish what an outbox row is for.
--
-- Every message Sashiko sends has so far been a review notification tied to a
-- patch, so the table could describe its own contents by whether patch_id was
-- set. Transactional mail such as a sign-in link belongs to a person rather
-- than a patch, and must be deduplicated, rate limited and observed
-- differently, so the purpose is recorded explicitly instead of inferred.
--
-- Existing rows are review notifications, which the default states without
-- rewriting the table: SQLite stores a constant default in the schema rather
-- than backfilling every row.
ALTER TABLE email_outbox ADD COLUMN kind TEXT NOT NULL DEFAULT 'review_notification';

CREATE INDEX IF NOT EXISTS idx_email_outbox_kind_created
    ON email_outbox(kind, created_at);
