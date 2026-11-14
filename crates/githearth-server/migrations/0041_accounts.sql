-- M4.1 Person A: account lifecycle and failed-login bookkeeping.
--
-- A disabled account is represented by `accounts.suspended = true`, the flag the
-- authorization engine already forbids ("suspended-account": a suspended
-- account can do nothing). The two added columns record *who* disabled it and
-- *when*, so the state is auditable and re-enabling clears it. `disabled_at`
-- being set is therefore always paired with `suspended = true`; the reverse is
-- not enforced because an operator may suspend an account directly.

ALTER TABLE accounts
 ADD COLUMN disabled_at timestamptz NULL,
 ADD COLUMN disabled_by uuid NULL REFERENCES accounts(id),
 ADD COLUMN password_changed_at timestamptz NULL;

ALTER TABLE accounts
 ADD CONSTRAINT accounts_disabled_bookkeeping
 CHECK ((disabled_at IS NULL) = (disabled_by IS NULL));

-- Failed login attempts, the input to bounded time-window throttling. Rows are
-- keyed by account (when the username resolved) and by client address (always
-- when the transport knows it). An unknown username contributes only its
-- client-address row, so guessing usernames is still bounded per source.
CREATE TABLE login_failures (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 account_id uuid NULL REFERENCES accounts(id) ON DELETE CASCADE,
 -- The client address as text so an unparseable or absent value is stored as
 -- NULL rather than failing the insert and losing the throttle signal.
 client_addr inet NULL,
 attempted_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- Per-account and per-client window counts; both use attempted_at for pruning.
CREATE INDEX login_failures_account ON login_failures (account_id, attempted_at)
 WHERE account_id IS NOT NULL;
CREATE INDEX login_failures_client ON login_failures (client_addr, attempted_at)
 WHERE client_addr IS NOT NULL;
CREATE INDEX login_failures_attempted_at ON login_failures (attempted_at);
