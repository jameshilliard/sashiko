-- Retires the analysis pipeline of bugs that were folded into another bug
-- before folding did that itself.
--
-- Folding used to write only the triage state, so the row kept whatever
-- pipeline state it had when the dedup stage returned. Those rows are stranded:
-- claiming now skips duplicates, so once startup recovery moves them to pending
-- nothing will ever pick them up again, and they fail the invariant that a
-- folded bug has left the pipeline.
--
-- Succeeded is the honest state. Reaching a duplicate is a completed triage
-- result, not a run that died, and the finding itself lives on the canonical
-- bug. Abandoned rows are left alone because an operator retired those
-- deliberately.
UPDATE bugs
   SET pipeline_state = 'succeeded',
       locked_by = NULL,
       lease_expires_at = NULL,
       updated_at = CAST(strftime('%s', 'now') AS INTEGER)
 WHERE lifecycle_status = 'duplicate'
   AND pipeline_state NOT IN ('succeeded', 'abandoned');
