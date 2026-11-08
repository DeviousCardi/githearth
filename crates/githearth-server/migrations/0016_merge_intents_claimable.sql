-- The merge worker claims every poll (issue #56); index only the claimable
-- states so the claim no longer scans every terminal intent.
CREATE INDEX merge_intents_claimable ON merge_intents (created_at)
    WHERE state IN ('queued', 'validating', 'executing');
