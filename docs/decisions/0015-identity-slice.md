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

- M4.1 adds the primary email with a uniqueness constraint, the
  `email_verifications` table (hashed, single-use, expiring token), the
  `password_resets` table of the same shape and the account's
  `must_change_password` state. An account therefore already has a verified
  address and a working administrator-initiated reset when this slice starts.
  What is still absent is self-service change of address, self-service
  recovery for someone who cannot reach an administrator, and every external
  identity: the account's only identifier besides the verified email is its
  username, and `audit_events.actor_id` is a non-null reference to an account,
  with an append-only trigger that rejects every update and delete.
- Secrets must live under the M4.2 envelope scheme, and deletion must be
  provable in backups, not only in the live database — within what that scheme
  actually encrypts.
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

The slice's new scope is **OIDC** sign-in and account linking, **change of
address**, **self-service recovery**, **second factors**, **service-account
administration** and **deletion**. Account creation, the primary email, the
verification token and the reset token remain M4.1's.

### Identity data

- The primary email, verification token, reset token and `must_change_password`
  are M4.1's (see Context) and are not redefined here. This slice consumes
  them: only a verified address may receive notifications, reset links or
  sign-up mail.
- This slice adds **change of address**: a new address is verified before it
  replaces the old one, the old address is notified, and an unverified new
  address changes nothing.
- This slice adds **self-service recovery**: an account that cannot reach an
  administrator requests a reset to its verified address, and a lost second
  factor is recovered through the same one-time token. Both reuse M4.1's token
  tables and add no second token scheme.
- An `external_identities` table maps `(issuer, subject)` to an account, with
  the provider's email and whether it was verified. An account may have several
  identities.
- Second factors: `totp_secrets` (encrypted), `webauthn_credentials` (public
  key, credential ID, sign count, transports) and single-use `recovery_codes`
  (hashed). A WebAuthn relying-party ID is the installation hostname; it is
  configuration, not per-organization, because a credential is bound to an
  origin.

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

- **Account:** tombstone the row in place; do not anonymize by update and do
  not row-delete. The row keeps its UUID, so `audit_events.actor_id` stays
  immutable and every existing audit row still resolves to a stable
  pseudonymous ID. Clear or destroy username, email, display name, password
  hash, second factors, sessions, access tokens, SSH keys and stored
  credentials. The append-only trigger gains no update or delete path, and no
  synthetic actor replaces the account. The tombstone is audited.
- **Organization:** tombstone the organization and remove its repositories,
  forks, trash, stored objects and quota. Audit rows keep their actor.
- **Backups:** every account and every organization has a data key under the
  M4.2 envelope scheme. Destroying that key makes the **secrets and encrypted
  columns** unreadable in older backups — authenticator seeds, tokens, OIDC
  client secrets and the rest of the wrapped set. It does not make a plaintext
  profile column unreadable: the pseudonymous account row, and the username,
  email and display name it held before deletion, remain readable in a backup
  taken before the deletion until that backup's retention expires. On the
  organization side the same key shreds its secrets and encrypted columns in
  older backups, while its repositories, forks and stored objects remain in a
  backup taken before deletion until that backup's retention expires. Backups
  taken before those keys exist cannot be shredded at all. Retention
  is the control for the profile columns and for pre-key backups, and the plan
  says so instead of promising more.
- Deletion across replicas is re-qualified in M5, which is why M5 takes it into
  its re-qualification drills; the semantics are fixed here.

### What is deliberately not in the slice

- SAML and SCIM, enterprise grouping, custom roles, access reviews and legal
  hold are the later part of M11.
- Personal namespaces are not introduced; organizations remain the only
  namespace.
- A hosted identity service, social login beyond OIDC, and hardware-attestation
  policy are out of scope.

## Consequences

- The tombstone changes no audit row, so the hash chain needs no new update
  path; the cost is that a pseudonymous row and its profile columns survive in
  backups until retention expires, which is the promise the plan makes.
- Crypto-shredding means a lost per-account key is unrecoverable data loss by
  design; key custody is the operational risk, and it is the same risk the
  installation key that M4.2 introduces already carries. It covers secrets and
  encrypted columns only.
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
4. The tombstone row's shape, what "cleared" means per column, and the
   retention of the pseudonymous row in the live database.
5. Data-key granularity: per account, per organization, and how a shared
   repository is keyed when ownership moves.
6. The exact `doctor` checks for the sign-up prerequisites.
