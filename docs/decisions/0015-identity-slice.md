# ADR 0015: Identity slice and account deletion

Date: 2026-11-15.

Status: **decision accepted for M11a; detailed design open.** Nothing here is
implemented. This is the ADR the milestone rule requires before the identity
slice starts; it depends on [ADR 0014](0014-platform-slice.md) for secrets,
tokens and principals.

## Context

M4.1 makes a team administrable by an administrator who creates accounts.
M11a is the step that lets an installation admit people itself, authenticate
them through an external provider, give them a second factor, recover them when
they lose a credential, and remove them when they leave — without an operator
at a SQL prompt. It is also the prerequisite for public sign-up, which is the
second product goal.

Three constraints shape the decision:

- The current schema has no email column, no verification or reset tokens, and
  no external identity table. `accounts.username` is the only identifier, and
  `audit_events.actor_id` is a non-null reference to an account, with an
  append-only trigger that rejects every update and delete.
- Secrets must live under the M4.2 envelope scheme, and deletion must be
  provable in backups, not only in the live database.
- An identity provider is an external dependency. An installation that cannot
  sign anyone in because the provider is down is worse than one without
  federation.

Alternatives considered: building sign-up inside M6 (wrong order — CI needs
authenticated users, not the reverse); password-only accounts with no
federation (fails the enterprise goal and doubles the recovery surface);
deleting rows and rewriting backups (impossible against an append-only audit
trail and immutable off-host copies); and a hosted identity service (violates
"no required vendor service").

## Decision

### Identity data

- An account gains a primary email with a uniqueness constraint and a
  verification state. A `email_verifications` table holds a hashed, single-use,
  expiring token. Only a verified address may receive notifications, reset
  links or sign-up mail.
- An `external_identities` table maps `(issuer, subject)` to an account, with
  the provider's email and whether it was verified. An account may have several
  identities.
- Second factors: `totp_secrets` (encrypted), `webauthn_credentials` (public
  key, credential ID, sign count, transports) and single-use `recovery_codes`
  (hashed). A WebAuthn relying-party ID is the installation hostname; it is
  configuration, not per-organization, because a credential is bound to an
  origin.
- A `password_resets` table holds a hashed, single-use, expiring token, the
  same shape as verification. The account carries `must_change_password`.

### Sign-in and linking

- OIDC is configured per installation: one issuer set, one client, one
  callback. Each organization has a policy for whether federation is offered,
  required for its members, or restricted to an email domain.
- Linking an external identity to an existing account requires a **verified**
  email that equals the provider's verified email. An unverified or absent
  email never links; a mismatch creates a new account or is refused, never a
  takeover. This is the account-takeover boundary and it is tested both ways.
- Local passwords remain valid unless an organization requires federation.
  A break-glass administrator login is documented and tested for the IdP-outage
  case.

### Sign-up

Off by default, in configuration and in the UI. When enabled:

- An address must be verified before any access; unverified accounts can do
  nothing but request another verification mail, subject to a per-address
  limit.
- Sign-up is rate-limited per address and per organization, with the velocity
  limits recorded in configuration. Organization creation is capped as M4.2
  already requires.
- The operator must have accepted the prerequisites: SPF/DKIM/DMARC and bounce
  handling, an abuse contact and takedown runbook, a sign-up velocity limit,
  and the deletion path below. `doctor` reports which are missing.
- A public multi-organization profile is not claimed as supported before this
  ships.

### Deletion

- **Account:** anonymize, do not row-delete. Clear username, email, display
  name, password hash, second factors, sessions, tokens and SSH keys; keep the
  audit rows with a system actor and a stable tombstone ID. The append-only
  trigger gains exactly one sanctioned anonymization path, audited itself.
- **Organization:** tombstone the organization and remove its repositories,
  forks, trash, stored objects and quota; keep the audit trail with a system
  actor.
- **Backups:** every account and every organization has a data key under the
  M4.2 envelope scheme. Destroying that key makes the ciphertext in older
  backups unreadable, which is what makes "deleted" true across immutable
  off-host copies. The live audit trail still resolves the actor through the
  tombstone ID.
- Deletion across replicas is re-qualified in M5; the semantics are fixed here.

### What is deliberately not in the slice

- SAML and SCIM, enterprise grouping, custom roles, access reviews and legal
  hold are the later part of M11.
- Personal namespaces are not introduced; organizations remain the only
  namespace.
- A hosted identity service, social login beyond OIDC, and hardware-attestation
  policy are out of scope.

## Consequences

- The account table and audit contract change in a way that touches every
  audit writer; the sanctioned anonymization path must be narrow and reviewed.
- Crypto-shredding means a lost per-account key is unrecoverable data loss by
  design; key custody is the operational risk, and it is the same risk the
  M4.2 installation key already carries.
- Federation makes the installation depend on an external issuer for the
  organizations that require it; the IdP-outage path is the mitigation, and it
  must be tested rather than assumed.
- Sign-up turns a private installation into an Internet-facing registration
  endpoint; the prerequisites are therefore gates, not documentation.

## Open items

1. The email provider interface and how bounce/complaint events reach the
   account state.
2. Recovery-code count and regeneration rules.
3. Per-organization federation policy inheritance and the domain-restriction
   matching rule.
4. The tombstone schema and how far an audit row may be reattributed.
5. Data-key granularity: per account, per organization, and how a shared
   repository is keyed when ownership moves.
6. The exact `doctor` checks for the sign-up prerequisites.
