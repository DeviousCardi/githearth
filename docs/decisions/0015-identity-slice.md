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
  callback. An organization's own identity provider on a shared installation
  is not in this slice; it arrives with SAML and SCIM in the later part of
  M11. Discovery and token requests go through the M4.2 outbound client. Each organization has a policy for whether federation is offered,
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
  synthetic actor replaces the account. The tombstone is audited. An account
  that is the last owner of an organization, or the last site administrator,
  cannot be deleted: the request is refused with the list of organizations
  until ownership is transferred or the organization is deleted.
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
- Retention of `data_key_directory` copies, not the tombstone, bounds a shred
  against an older archive: a custody copy taken before a deletion still holds
  the key and un-shreds every archive it can open.

## Resolutions recorded by M11a increment 8A (infrastructure)

### Data-key granularity (open item 5)

One data key per account and one per organization, generated on first use. The
id is deterministic from the owner: SHA-256 over `domain || kind || 0x00 || 16
raw UUID bytes` with the UUID version and variant bits set (`UUIDv5`-shaped,
not SHA-1 UUIDv5; call `data_keys::id_for`, do not reimplement it). It is the
reference the owner row carries: `accounts.data_key_id` and
`organizations.data_key_id` (the identity-schema lane owns those columns; the
infrastructure lane owns the key material and returns the id from
`data_keys::ensure_account_data_key` / `ensure_organization_data_key`). The
column is informational and is never the shred input. A
shared repository does not get its own key in this slice: repository secrets
are not in the schema yet, and when they arrive they can follow the
organization owning the repository.

A secret belonging to an owner is encrypted under a per-secret data key, which
is wrapped by the owner key; the owner key is wrapped by the installation key.
Installation-level secrets (the audit IP key) keep the M4.2 shape and are the
pre-key rows a pre-key backup cannot shred.

### Where owner keys live, and the divergence from ADR 0014's audit-key resolution

ADR 0014's resolution for the audit IP key keeps it in PostgreSQL as an
envelope, and that is right for row-level shredding of the live digest binding.
It cannot make an **older backup** unreadable, because the backup carries the
wrapped key and the installation key opens it. Crypto-shredding across backups
therefore requires the key to be absent from the archive. Owner keys live in
`data_key_directory` (default: `data-keys` beside `installation_key_file`;
`GITHEARTH_DATA_KEY_DIRECTORY`), wrapped by the installation key. The directory
is outside `storage_root`, is not in the database or an archive, and is the
operator's custody problem exactly as the installation key already is — which
is the risk this ADR's Consequences section records. This **extends** ADR 0014
§3's wrapped set with a key store outside the database; the envelope, AAD and
rotation construction are unchanged, and rotation re-wraps owner keys too.

### Shredding and its exact scope

Destroying an owner key writes a tombstone and removes the key file (tombstone
first, so an interrupted shred is still a shred). Every secret and encrypted
column under that key becomes unreadable in the live database and in every
backup whose custody copy no longer holds the key. A retained custody copy that
predates the deletion un-shreds the archives it can open, because it has
neither the tombstone nor the removal; removing the key from every retained
copy is what closes that. The pseudonymous account/organization row and
its plaintext profile columns remain in a pre-deletion backup until that
backup's retention expires. A backup predating the owner keys has nothing to
shred: its secrets are wrapped directly by the installation key, so retention
is the only control. An account with no owner key at deletion records
`had_key = false` rather than pretending the older secret was shredded. These
limits are written where the operator reads them
(`docs/operations/secrets-at-rest.md` and `docs/operations/backup-restore.md`).

### Backup and restore

The manifest lists owner keys as `{key_id, owner_kind, owner_id, kek_id,
algorithm, wrap_method, shredded, had_key}`; it never contains a key or a
wrapped key, and `had_key = false` marks a pre-key tombstone. `restore` opens
owner keys from the custody directory: a present key opens, an absent key
without a tombstone fails before anything is published (a lost key cannot
silently become an unreadable installation), and a tombstoned key is a
completed shred that restore **refuses by default** and accepts only with
`--accept-shredded`, naming the key ids either way. `secrets status` and
`doctor` make the same distinction.

## Open items

1. The email provider interface and how bounce/complaint events reach the
   account state.
2. Recovery-code count and regeneration rules.
3. Per-organization federation policy inheritance and the domain-restriction
   matching rule.
4. The tombstone row's shape, what "cleared" means per column, and the
   retention of the pseudonymous row in the live database.
5. Data-key granularity: per account, per organization, and how a shared
   repository is keyed when ownership moves. (Resolved for accounts and
   organizations by increment 8A above; repository secrets are not in this
   slice.)
6. The exact `doctor` checks for the sign-up prerequisites.
