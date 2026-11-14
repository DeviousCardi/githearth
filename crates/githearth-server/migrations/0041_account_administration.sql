-- Account administration (M4.1, acceptance cases U01-U03).
--
-- Account state reuses the existing `accounts.suspended` flag: login, bearer
-- sessions, access tokens, SSH keys and Git admission already consult it, so a
-- disabled account is refused by every path without a second mechanism.
--
-- Failed sign-ins are counted per username and per client address inside a
-- sliding window. A username is stored as a SHA-256 of its normalized spelling,
-- so the table does not record what was typed; rows hold no password. They are
-- pruned once they are older than any configured window, so a counter can never
-- lock an account permanently.
CREATE TABLE login_attempts (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 -- 'username' (hashed) or 'client_ip'; the two bounds are counted independently.
 scope text NOT NULL CHECK (scope IN ('username', 'client_ip')),
 -- The normalized value the bound applies to (hashed for usernames, text for addresses).
 subject text NOT NULL CHECK (length(subject) BETWEEN 1 AND 128),
 attempted_at timestamptz NOT NULL
);
CREATE INDEX login_attempts_scope_window ON login_attempts (scope, subject, attempted_at);
-- The retention prune deletes by age across both scopes.
CREATE INDEX login_attempts_age ON login_attempts (attempted_at);
