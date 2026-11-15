# Accounts

An installation starts with the single administrator created by
`githearth-server bootstrap-admin`. Every further account is created by an
administrator: through the web UI at `/admin/accounts`, or through the JSON API
described below. No SQL and no separate command are needed.

## Administrator capabilities

| Action | UI | API |
| --- | --- | --- |
| List accounts | `/admin/accounts` | `GET /api/v1/admin/accounts?after=<uuid>&limit=<1..100>` |
| Create an account | form on `/admin/accounts` | `POST /api/v1/admin/accounts` |
| Disable an account | "Disable" on `/admin/accounts` | `POST /api/v1/admin/accounts/{id}/disable` |
| Re-enable an account | "Enable" on `/admin/accounts` | `POST /api/v1/admin/accounts/{id}/enable` |
| Promote to site administrator | "Promote" on `/admin/accounts` | `POST /api/v1/admin/accounts/{id}/promote` |
| Demote from site administrator | "Demote" on `/admin/accounts` | `POST /api/v1/admin/accounts/{id}/demote` |
| Reset a password | "Reset password" on `/admin/accounts` | `POST /api/v1/admin/accounts/{id}/password-reset` |

The API requires a **session** Bearer token belonging to an enabled
administrator. A personal access token cannot administer accounts, and neither
can a non-administrator: both receive `403 {"error":"not permitted"}`, the same
refusal every administrator-only action uses. These actions are sensitive, so
the session must also have authenticated within the last 15 minutes; an older
session receives `403 {"error":"reauthentication required","reason":"sensitive"}`.
`POST /api/v1/me/reauthenticate` with `{"password":"…"}` refreshes the session's
freshness window, and `/settings` offers the same prompt.

The same guard protects promotion and demotion as disabling: an account cannot
be promoted while disabled, and the last enabled administrator cannot be
demoted. Concurrent demotions and disables take one advisory lock, so at least
one enabled administrator always remains.

Creating an account takes the username and the initial password:

```
curl -sS -X POST https://githearth.example/api/v1/admin/accounts \
  -H "Authorization: Bearer $SESSION_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"username":"alice","password":"the initial password"}'
```

The response contains the account id, username, `is_admin`, `suspended`,
`must_change_password` and `created_at` — never the password or its hash. A password is never generated,
logged, emailed or returned: the administrator supplies it and passes it to the
account owner out of band. Usernames follow the bootstrap rule: 3–64 lowercase
ASCII letters, digits, underscores or hyphens. Passwords are 12–1024 bytes and
are hashed with the same Argon2id parameters as every other password.

Disabling an account sets the existing `accounts.suspended` flag, deletes the
account's sessions and retires any live password-reset code in one transaction.
While disabled, the account is refused at sign-in and denied new admission on
every path: bearer sessions, personal access tokens, the browser UI, Git over
HTTP and Git over SSH. Re-enabling restores sign-in; it does **not** restore the
sessions that were revoked when the account was disabled (personal access
tokens and SSH keys are left untouched and work again), and a reset code issued
before the disable stays dead.

Two disable rules protect the installation:

- an administrator cannot disable their own account, and
- the last **usable** administrator cannot be disabled. A reset-pending
  administrator cannot sign in, so it does not count as the administrator that
  remains.

Both answer `409` and change nothing. The demote API applies the same rules:
an administrator cannot demote their own account, and the last usable
administrator cannot be demoted. Concurrent requests to disable or demote two
administrators are serialized, so at least one usable administrator always
remains. The account list shows `reset pending` for an account with an
outstanding reset code.

## Password change and reset

Any account owner can change their own password at `/settings` (or
`POST /api/v1/user/password` with `{"current_password":…,"new_password":…}`).
The current password is required. A wrong current password is counted against
the same failed sign-in bounds as a wrong login, so the form cannot be used to
guess a password faster than sign-in allows.

Both a change and an administrator reset store the new hash and revoke the
account's live sessions, access tokens and SSH keys in one transaction. A
password change keeps the session that made it, so the person who changed it
stays signed in; every other credential of the account is revoked.

An administrator reset never accepts or reveals a password. It returns a
single-use code (valid for 60 minutes) that the administrator gives to the
account owner out of band, and sets `must_change_password`. Until the code is
redeemed the account signs in nowhere: sessions, access tokens, SSH keys, the
browser, the API, Git over HTTP and Git over SSH are all refused. The account
owner redeems the code in the browser at `/reset` (linked from `/login` as
"Have a reset code?") or at `POST /api/v1/password-reset/redeem` with
`{"username":…,"code":…,"new_password":…}`. Either route stores the owner's own
password, clears the flag and retires every outstanding code. A used,
superseded or expired code is refused with the same fixed message, and the code
is never logged or stored in plaintext.

If an administrator has lost their password and no other administrator can
sign in, restore them from the host. The password is read from stdin, never
from the command line:

```
printf '%s\n' "$NEW_PASSWORD" | githearth-server reset-admin --username alice
```

This restores an existing account as an enabled site administrator and revokes
its sessions, access tokens, SSH keys and reset codes. It is audited as
`administrator.reset`.

## Failed sign-in throttling

Failed sign-ins are recorded in PostgreSQL and bounded per username **and** per
client address inside a sliding window:

| Setting | Default | Meaning |
| --- | --- | --- |
| `login_throttle_max_per_username` | `10` | Failed sign-ins allowed for one username in the window |
| `login_throttle_max_per_client` | `50` | Failed sign-ins allowed from one client address in the window |
| `login_throttle_window_seconds` | `900` | Window length in seconds |

The same counters cover the JSON login, the browser sign-in form and Git
password authentication. The client address is resolved exactly as for the
audit log, so `trusted_proxies` applies and an untrusted client cannot spoof it
with `X-Forwarded-For`.

The bounds are expressed as a burst of attempts inside a window, not as a
lockout:

- a throttled sign-in is refused **before** the password is hashed, with
  `429 {"error":"too many sign-in attempts; try again later"}` (the browser
  shows "Too many sign-in attempts");
- the refusal is identical for an existing and an unknown username, so it
  reveals nothing about which accounts exist;
- a successful sign-in clears that username's counter;
- attempts outside the window stop counting, so waiting out the window always
  restores access — no account can be locked permanently;
- a refused attempt is not recorded, so throttling never extends itself;
- the recorded rows hold a SHA-256 digest of the trimmed, lower-case username
  (not salted; the username is not a secret) and the resolved address, never a
  password, and rows older than a day are pruned.

An administrator whose sign-in is throttled can wait for the window, sign in
from another address (the per-address bound), or have another administrator
reset the password: a reset clears the account's failed-attempt counter.

## Audit records

Creating, disabling, re-enabling, promoting, demoting, changing and resetting
a password each write their audit record and outbox event in the same
transaction as the change (`account.created`, `account.disabled`,
`account.enabled`, `account.promoted`, `account.demoted`,
`account.password_changed`, `account.password_reset`,
`account.password_reset_redeemed`, `administrator.reset`). Payloads carry the
account id and, where applicable, how many sessions, access tokens, SSH keys
and reset codes were revoked — never a password, a reset code or a password
hash.

## Related

- [Teams and invitations](teams.md): adding an account to a team or an
  organization by invitation; an invitation only accepts a verified address.
- [Email](email.md): verifying an address and the `email` doctor check.
- [Repository restore](repositories.md): the `--actor` account a restore names.
