-- Pull-request head pins: restore the documented retention on 0045's backfill.
--
-- 0045 pinned the head of every pull request that predated pins with
-- `now() + interval '30 days'`, while the plan, ADR 0013, the configuration
-- default and the example TOML all promise
-- `housekeeping_pin_retention_days` (90 days by default) after close or merge.
-- On an upgraded installation those historical pins therefore expired three
-- times sooner than the case promises. 0045 is applied history whose checksum
-- scratch databases hold, so it is not edited; this migration corrects the
-- rows it wrote.
--
-- The extension is anchored to the pin's own `created_at`, not to the instant
-- this migration happens to run, so a row 0045 wrote gets the window it should
-- have had rather than a fresh window from the upgrade. Only rows that are
-- still live are touched:
--
--   * `retain_until` NULL means "while the subject row exists" (0046's other
--     pin kinds) and is left alone;
--   * a pin whose `retain_until` is already in the past is left expired. It is
--     not live, a prune may already have released its objects, and extending
--     it now would retain objects past the stated window rather than repair
--     the window that was promised;
--   * pins that are not 0045 backfills (reason is not `backfill`, or the pin
--     has no `pull_request_id`) are left alone.
--
-- A SQL migration cannot read the server's configured retention: the value
-- lives in the installation's configuration file, not in PostgreSQL. 90 is the
-- documented default this migration restores. The `migrate` and `setup`
-- commands then call `pins::extend_backfilled_pins` with the loaded
-- configuration, which raises a still-live backfill pin to a larger configured
-- window; it never shortens one.
UPDATE pinned_commits
SET retain_until = created_at + interval '90 days'
WHERE reason = 'backfill'
  AND pull_request_id IS NOT NULL
  AND retain_until IS NOT NULL
  AND retain_until > now()
  AND retain_until < created_at + interval '90 days';
