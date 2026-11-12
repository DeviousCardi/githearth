-- M3: threaded inline review comments and suggestions.
--
-- A comment on a diff is anchored to a commit, path and line, plus the side of
-- the diff it points at (the old/base side or the new/head side). Only an
-- anchored comment names a side. A thread's root comment tracks whether the
-- thread is resolved; a reply is never resolved on its own.

ALTER TABLE pull_request_comments
    ADD COLUMN side text NULL CHECK (side IN ('old', 'new')),
    ADD COLUMN resolved_at timestamptz NULL,
    ADD COLUMN resolved_by uuid NULL REFERENCES accounts(id);

ALTER TABLE pull_request_comments
    ADD CONSTRAINT pull_request_comments_side_anchor_check
    CHECK (side IS NULL OR commit_id IS NOT NULL);

ALTER TABLE pull_request_comments
    ADD CONSTRAINT pull_request_comments_resolution_pair_check
    CHECK ((resolved_at IS NULL) = (resolved_by IS NULL));

ALTER TABLE pull_request_comments
    ADD CONSTRAINT pull_request_comments_resolution_root_check
    CHECK (resolved_at IS NULL OR in_reply_to IS NULL);
