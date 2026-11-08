CREATE TABLE pull_request_review_requests (
    pull_request_id uuid NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    reviewer_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    requested_by uuid REFERENCES accounts(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (pull_request_id, reviewer_id)
);

ALTER TABLE notifications DROP CONSTRAINT notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check
    CHECK (kind IN ('pull_request.merged', 'pull_request.reviewed', 'pull_request.review_requested'));
