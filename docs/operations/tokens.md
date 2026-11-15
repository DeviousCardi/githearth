# Scoped tokens and the legacy deprecation window

This page is the operator's view of ADR 0014 section 2 and 2a. It covers who can
issue a token, what confines it, and when the old two-scope tokens stop working.

## Subjects

| Subject | Who issues it | What it is |
| --- | --- | --- |
| `account` | The account, over a session | A person's token. With a resource set it is narrowed; without one it is the legacy token. |
| `service_account` | An owner of the service account's organization | An organization-owned non-human identity. It has its own grants, audit actor and SSH keys. |
| `run` | The account that triggered the run, while the run is live | One CI run. It inherits the triggering account's grants, resolves to the run and that account, and is refused the moment the run ends. |

## Issuance

`POST /api/v1/user/tokens`, authenticated with a session (not with a token),
returns the plaintext exactly once. A scoped request carries a `resource_set`,
`scopes`, `audience` and an expiry:

```json
{
  "name": "ci-build",
  "subject": {"kind": "account"},
  "resource_set": [
    {"type": "repository", "id": "00000000-0000-0000-0000-000000000000"},
    {"type": "package", "organization": "acme", "name": "server"}
  ],
  "scopes": ["repo:read"],
  "audience": ["api", "git"],
  "expires_in_seconds": 3600
}
```

- `repo:read` / `repo:write` cover repository actions; `repo:write` implies
  read. `package:read` / `package:write` cover the M7 registry and never permit
  a repository action.
- `api`, `git` and `registry` are the audiences. A token presented on another
  transport is refused before authorization.
- A scoped token expires in 60 seconds to 24 hours (default one hour).
- Delete it with `DELETE /api/v1/user/tokens/{id}`; revocation is checked on
  every admission, so it takes effect immediately on the API and Git HTTP.

## Delivery to a job

A job receives the plaintext in a mode-`0600` file whose path is exported as
`GITHEARTH_TOKEN_FILE`, plus `GITHEARTH_TOKEN` for tools that need it inline.
The runner masks the inline value in its logs and deletes the file when the run
ends. M6 owns the runner; the contract is recorded here and in ADR 0014.

## The legacy window

The first two-scope tokens (`repo:read` / `repo:write`, no resource set) are
deprecated from **2026-11-16**. They keep working through **2027-06-30** and are
refused from **2027-07-01T00:00:00Z**.

Where an operator reads it:

- `githearth-server doctor` prints a `tokens` check with the enforced cutoff and
  the number of legacy tokens still accepted.
- The configuration value `legacy_token_deprecation_date` (environment
  `GITHEARTH_LEGACY_TOKEN_DEPRECATION_DATE`, default `2027-06-30`) is the date
  the server enforces. It must be a real calendar date in
  `2026-11-16..=2027-06-30`: it can shorten the window, never extend it.
- A legacy token's `legacy_deadline` column stores the first instant of the UTC
  day after that date (default `2027-07-01T00:00:00Z`), and the token is refused
  at and after it even if it has no `expires_at`. A legacy row cannot exist
  without that instant.
- At start-up the server only shortens: `legacy_deadline = LEAST(deadline,
  cutoff)`. Editing the configuration back to a later date does not resurrect a
  token that has already been narrowed.

During the window a legacy token has exactly its old authority: its scopes and
expiry are unchanged, and it is not confined to a resource set. Replace it with
a scoped token before the deadline. Tokens created without a `resource_set`
during the window are legacy tokens and share the same deadline; after the
cutoff the server refuses to issue a legacy token at all.
