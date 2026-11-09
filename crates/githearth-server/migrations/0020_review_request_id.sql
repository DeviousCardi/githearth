-- Identify each review request, so a notification job can check that the exact
-- request it was enqueued for still exists, not just one for the same reviewer
-- (a remove followed by a re-request must not notify twice; #86).
ALTER TABLE pull_request_review_requests
    ADD COLUMN id uuid NOT NULL DEFAULT gen_random_uuid() UNIQUE;
