# Email: SMTP, verification and notifications

GitHearth sends plain-text mail only. It never opens a socket until SMTP is
configured, and every send runs in a durable job, never in the request that
produced the event.

## Configuration

| TOML key | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `smtp_host` | `GITHEARTH_SMTP_HOST` | empty | SMTP server; empty disables delivery |
| `smtp_port` | `GITHEARTH_SMTP_PORT` | `587` | SMTP port |
| `smtp_tls` | `GITHEARTH_SMTP_TLS` | `starttls` | `starttls`, `implicit` (port 465) or `none` (no TLS; refused with `smtp_username`) |
| `smtp_from` | `GITHEARTH_SMTP_FROM` | empty | `From:` address; required when `smtp_host` is set |
| `smtp_bounce_address` | `GITHEARTH_SMTP_BOUNCE_ADDRESS` | empty | Envelope sender (`MAIL FROM`) for bounces; falls back to `smtp_from` |
| `smtp_dkim_selector` | `GITHEARTH_SMTP_DKIM_SELECTOR` | `default` | Selector `doctor` checks at `<selector>._domainkey.<domain>` |
| `abuse_contact` | `GITHEARTH_ABUSE_CONTACT` | empty | Published abuse address; required before sign-up |
| `email_dns_check_enabled` | `GITHEARTH_EMAIL_DNS_CHECK_ENABLED` | `true` | Whether `doctor` resolves SPF, DKIM and DMARC |
| `smtp_username` | `GITHEARTH_SMTP_USERNAME` | unset | AUTH username; requires a credential file |
| `smtp_password_file` | `GITHEARTH_SMTP_PASSWORD_FILE` | unset | File containing the SMTP credential, or `-` for one line on stdin |
| `public_url` | `GITHEARTH_PUBLIC_URL` | `http://127.0.0.1:8080` | Base of links in mail |

The credential is **never a TOML value and never an argument**. Put it in a
file readable only by the `githearth` user:

```bash
install -m 0600 /dev/null /etc/githearth/smtp-password
printf '%s\n' "$SMTP_PASSWORD" > /etc/githearth/smtp-password
unset SMTP_PASSWORD
```

`serve` reads the file once at start-up. An unreadable file fails start-up with
a message naming no contents. The credential is not returned by an API, is not
written to the database, and is not included in a log line, a job `last_error`
or an HTTP error body. The transport's `Debug` output redacts it.

A minimal authenticated STARTTLS configuration:

```toml
smtp_host = "smtp.example.com"
smtp_port = 587
smtp_tls = "starttls"
smtp_from = "githearth@example.com"
smtp_username = "githearth"
smtp_password_file = "/etc/githearth/smtp-password"
public_url = "https://githearth.example"
```

An unauthenticated relay omits `smtp_username` and `smtp_password_file`.
`implicit` wraps the connection in TLS before the greeting; `none` is
cleartext and belongs only on a loopback development relay. `none` is refused
at start-up when `smtp_username` is set, because the client's `AUTH PLAIN`
would put the password on the wire in the clear; an authenticated relay must
use `starttls` or `implicit`. An unauthenticated cleartext relay is allowed
because it carries no credential. The server certificate is verified against
the public web PKI; there is no "accept any certificate" switch.

SMTP resolves its host once and checks every answer with the outbound client's
address policy: a cloud-metadata endpoint is always refused, and a loopback,
link-local or private answer needs `outbound_allow_private = true`. A loopback
development relay therefore needs that option as well as `smtp_host`.

## SPF, DKIM, DMARC and bounces

A public host that enables sign-up must publish all three records for the
sending domain and accept bounces. This is the M4.2 abuse baseline; M11a's
sign-up and recovery depend on it.

- **SPF** — a TXT record at the domain apex beginning `v=spf1`, naming the
  relay that sends the mail (for example `v=spf1 include:mail.example
  ~all`). Do not use `+all`.
- **DKIM** — a TXT record at `<selector>._domainkey.<domain>` beginning
  `v=DKIM1`, with the public key. Set `smtp_dkim_selector` to the same
  selector the relay signs with. The built-in transport does not sign; the
  relay must, or an operator can front it with a signing relay.
- **DMARC** — a TXT record at `_dmarc.<domain>` beginning `v=DMARC1`, with a
  policy of at least `p=quarantine` (prefer `p=reject`) and an `rua=` address
  the operator reads.
- **Bounce address** — set `smtp_bounce_address` to a mailbox that accepts
  bounces. It is used as `MAIL FROM`; `From:` stays `smtp_from`. A bounce is a
  delivery failure the operator must be able to see, not a message that
  disappears into the sending mailbox.

`doctor` has an **email DNS** check: with SMTP configured it looks up those
three TXT records and requires `smtp_bounce_address` and `abuse_contact`. A
resolved domain missing a record fails the check; a resolver outage warns. The
check is read-only and sends no mail. It can be disabled with
`email_dns_check_enabled = false` for an offline preflight, but that does not
waive the requirement before sign-up.

## Rate limits

Email is bounded so one tenant or one address cannot use the installation as a
bulk sender:

| Budget | Default | Where enforced |
| --- | --- | --- |
| Notification mail per organization | 200/hour | `notification_email::deliver` |
| Security mail per destination address | 5/hour | `email_verification::send_requested` |

A send over its budget returns the durable job to `pending` with a delay; the
intent is not dropped. The counters are in-process and reset with the server,
which is the same lifetime as the in-memory request budgets (see
[limits](limits.md)).

## What is sent

Notification mail is limited to the notification kinds below and is addressed
only to the account's **verified** address. Each message is plain text, carries
no token, no secret and no other account's data, and links to the recipient's
own pull request (reading it still enforces access):

| Kind | Subject | Body |
| --- | --- | --- |
| `pull_request.merged` | `[GitHearth] Pull request #N merged` | "Your pull request #N in org/repo was merged." plus the link |
| `pull_request.reviewed` | `[GitHearth] New review on pull request #N` | "Your pull request #N in org/repo received a review." plus the link |
| `pull_request.review_requested` | `[GitHearth] Review requested on pull request #N` | "You were asked to review pull request #N in org/repo." plus the link |

The one transactional message is the verification link:

| Message | Subject | Body |
| --- | --- | --- |
| Email verification | `[GitHearth] Verify your email address` | "Confirm that this address belongs to your GitHearth account by opening: …/verify-email/TOKEN. This link works once and expires in 24 hours. If you did not request it, ignore this message." |

The verification link is the mechanism by which an address becomes verified; it
grants address verification only, never a session, and is single-use and
expiring. Notification mail never contains it.

Every message carries a `Date:` header stamped at send time and a `Message-ID:`
built from the durable intent or verification row id. A retry of the same intent
or request therefore reuses the id, and a receiving server can deduplicate the
redelivery; a new request creates a new row and a new id.

## Verification flow

1. The account owner requests a link at `POST /api/v1/me/email-verification`
   with `{"email":"alice@example.com"}`, or an administrator uses
   `POST /api/v1/admin/accounts/{id}/email-verification`.
2. The address is stored as the account's primary address while unverified. A
   duplicate address is refused by the unique index as
   `409 {"error":"email address is already in use"}`; the refusal names no
   account. Replacing an already verified address is M11a's change-of-address
   flow and is refused here.
3. The outbox consumer mints a token, stores only its SHA-256 and sends the
   link. The response is `202` with `expires_at`, never the token.
4. `POST /api/v1/email-verification/redeem` with `{"token":"…"}` or the form at
   `/verify-email/{token}` sets `email_verified_at` once. An expired, reused or
   wrong-address token answers the same `401` refusal.
5. Request and redemption are audited as
   `account.email_verification_requested` and `account.email_verified`.

The verification row records the address it was minted for. A link requested
before an address change cannot verify the new address.

## Preferences and deduplication

`GET|PUT /api/v1/user/notification-preferences` controls which notification
kinds are delivered by email. A missing row means enabled; a muted kind writes
no email intent at all. Every change is audited. Preferences do not mute the
verification message.

Each in-app notification gets at most one `email_intents` row, unique on the
notification. The same event delivered twice, or two events naming the same
source row, produce one email. A failed send retries the same durable
`notification.outbox` delivery from that intent; once `sent_at` is set, a later
duplicate sends nothing. A delivery that exhausts its budget is `dead` and
`githearth-server jobs requeue` recovers it.

## Preflight: the `email` check

`githearth-server doctor` runs one read-only `email` check. It opens no socket
and sends no mail. With no `smtp_host` it reports that delivery is disabled; if
`smtp_from`, `smtp_username` or `smtp_password_file` is set while the host is
empty, it warns that they are ignored. With an `smtp_host` it:

- checks that `smtp_from` is set and is a valid address;
- checks that `smtp_username`, when set, has a credential file, and that the
  file exists, is readable and is not empty (a `-` credential is read from
  stdin at start-up; `doctor` warns that it cannot verify it without consuming
  it);
- warns when the TLS mode does not match the port: `implicit` with a port
  other than 465, or `starttls`/`none` on 465.

A missing or unreadable credential fails the check, because `serve` reads the
same file and would refuse to start. A TLS/port mismatch only warns: a relay on
a non-standard port may still work. The check cannot verify the credential
against the server, the certificate chain, SPF/DKIM/DMARC, or that the relay
will accept a message; only a real send proves those. See the check list in
[install.md](install.md).

## Things this slice deliberately does not do

- **No SPF/DKIM/DMARC or bounce handling.** M4.2's abuse baseline owns those and
  M11a requires them before sign-up. This slice sends through the operator's
  relay with the configured envelope sender.
- **No sign-up mail.** Public sign-up is M11a. An unverified address receives
  no notification, no reset link (administrator resets return a one-time code
  to the administrator) and no sign-up mail.
