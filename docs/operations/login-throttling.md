# Failed-login throttling

Every password verification — the JSON `POST /api/v1/login`, the browser sign-in
form, and Git HTTP Basic authentication — records a failed attempt and is
subject to the same bounded, time-window throttle (M4.1 U03). Throttling applies
before the password hash is computed, so a locked-out target costs no hashing
work.

## Counters and limits

An attempt is refused with HTTP `429` when either counter reaches its limit
inside the window:

| Counter | TOML key | Environment override | Default | Range |
| --- | --- | --- | --- | --- |
| One account | `login_throttle_account_limit` | `GITHEARTH_LOGIN_THROTTLE_ACCOUNT_LIMIT` | 5 | 1–100 |
| One client address | `login_throttle_client_limit` | `GITHEARTH_LOGIN_THROTTLE_CLIENT_LIMIT` | 20 | 1–1000 |
| Window | `login_throttle_window_seconds` | `GITHEARTH_LOGIN_THROTTLE_WINDOW_SECONDS` | 900 | 1–86400 |

`login_throttle_client_limit` must not be below `login_throttle_account_limit`,
so a shared address (a NAT or a busy CI host) is never more restrictive than a
single account. The client address is resolved exactly as the request log does:
the peer address, or the first untrusted `X-Forwarded-For` entry when the peer
is a configured `trusted_proxies` entry (see [foundation.md](../foundation.md)).
When no address is known, only the account counter applies.

An unknown username contributes a client-address failure but no account
failure, so guessing usernames is still bounded per source.

## Why it cannot lock anyone out permanently

Counts are a sliding window: a failure stops counting once it is older than
`login_throttle_window_seconds`, so the lockout clears by itself without
operator action. A refused attempt is **not** recorded, so an attacker hammering
the endpoint cannot keep the window alive. A successful authentication clears
that account's recorded failures, and so does an administrator password reset.
Creating an account, or reading it, never depends on the throttle.

The administrator is treated like any other account — throttling never grants a
bypass — but the expiring window means a brute-force attempt cannot lock the
administrator (or anyone) out permanently.

## Storage

Failures live in `login_failures`, one row per attempt, keyed by `account_id`
(when the username resolved) and `client_addr`. Each recorded failure also
deletes rows older than the window, so the table stays bounded by the failure
rate times the window. The table survives restart and restore like any other
metadata.

## Tuning

- A stricter site: lower `login_throttle_account_limit` and
  `login_throttle_client_limit`, or lengthen the window.
- A shared-NAT site: raise `login_throttle_client_limit` above the account
  limit so legitimate users behind one address are not throttled together.
- A CI clone host that authenticates often: keep the window short, or allow
  machine accounts a personal access token, which the Git HTTP path validates
  without a password check and therefore without a failure count.
