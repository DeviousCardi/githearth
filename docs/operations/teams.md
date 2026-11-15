# Teams and invitations

Teams group organization members and carry repository (or organization)
grants. Invitations add an account to an organization, optionally through a
team, by email. Both are administered by an organization owner; neither needs
SQL or a host command.

## Teams

| Action | UI | API |
| --- | --- | --- |
| List teams | `/organizations/{slug}/teams` | `GET /api/v1/organizations/{org}/teams` |
| Create a team | form on the teams page | `POST /api/v1/organizations/{org}/teams` with `{"slug":…,"name":…}` |
| Delete a team | "Delete" on a team page | `DELETE /api/v1/organizations/{org}/teams/{team}` |
| List members | team page | `GET /api/v1/organizations/{org}/teams/{team}/members` |
| Add a member | form on a team page | `POST /api/v1/organizations/{org}/teams/{team}/members` with `{"username":…}` |
| Remove a member | "Remove" on a team page | `DELETE /api/v1/organizations/{org}/teams/{team}/members/{username}` |
| List grants | team page | `GET /api/v1/organizations/{org}/teams/{team}/grants` |
| Grant access | form on a team page | `POST /api/v1/organizations/{org}/teams/{team}/grants` |
| Revoke a grant | "Revoke" on a team page | `DELETE /api/v1/organizations/{org}/teams/{team}/grants/{grant}` |

A team slug follows the organization slug rules (lowercase ASCII letters,
digits and hyphens, at most 64 bytes). Adding a member requires that account to
be a member of the organization already; adding someone twice changes nothing.
Team membership is not a grant by itself: the account exercises the grants the
team holds, so adding a member is a `team.update` change and is audited.

A grant is either a repository role — `read`, `triage`, `write`, `maintain` or
`admin` — or, with `"on_organization": true`, an organization role `owner` or
`member`. The request for a repository grant is
`{"repository":"name","role":"write"}`; for an organization grant it is
`{"on_organization":true,"role":"member"}`. Grants are never edited: granting
the same access twice returns the active row, and revoking retires it.

`team.create`, `team.update` and `team.delete` are owner actions and are
`sensitive`, so a browser or API session must have authenticated within the
last 15 minutes. The same guard covers adding members and grants. A team cannot
be deleted if the only active grant it holds is the organization's last owner
grant, because that would leave the organization with no owner. Deleting a team
revokes its grants (never deletes their rows) and removes its memberships in
the same transaction.

## Invitations

An invitation names one organization, an email address, an organization role
(`owner` or `member`), optionally a team, and an expiry of 1–30 days (7 by
default). Creating one is an owner action (`invitation.create`, `sensitive`).
The token is shown once to the inviter, who sends the link to the address out
of band; this release does not send invitation mail itself (the address still
has to be verified before the invitation can be accepted; see
[email.md](email.md)).

| Action | UI | API |
| --- | --- | --- |
| List invitations | `/organizations/{slug}/invitations` | `GET /api/v1/organizations/{org}/invitations` |
| Create an invitation | form on the invitations page | `POST /api/v1/organizations/{org}/invitations` |
| Revoke an invitation | "Revoke" on the invitations page | `DELETE /api/v1/organizations/{org}/invitations/{id}` |
| Preview an invitation | `/invitations/{token}` | `GET /api/v1/invitations/{token}` |
| Accept an invitation | same page, signed in | `POST /api/v1/invitations/{token}` |

The create body is
`{"email":"alice@example.com","role":"member","team":"optional-team-slug","expires_in_days":7}`.
The response is the only one that carries the token (64 lowercase hex
characters); the row stores only its SHA-256, and no audit event, log line or
idempotency record keeps the plaintext. Creation deliberately bypasses the
idempotency store because that store would keep the response body containing
the token: a repeated request creates a second, equally valid invitation rather
than replaying the first response.

Acceptance requires the recipient to be signed in and to have **verified** the
invited email address (`accounts.email_verified_at`). The token is the
credential; it is single-use and expires. A valid acceptance creates the
organization membership, its grant, the team membership and the team's grants
in one transaction, writes `organization.invitation_accepted`, and consumes the
row. A wrong account, an unverified address, an expired or revoked token, an
already used token and an unknown token all answer the same `404
{"error":"invitation not found"}`, so the token never discloses an organization
to someone it was not addressed to. An already accepted invitation cannot be
revoked.

Invitations are audited as `organization.invitation_created`,
`organization.invitation_accepted` and `organization.invitation_revoked`. The
created payload names the invitation and the invited address; the accepted
payload names the invitation, role and team; the revoked payload names only the
invitation. None of them carries the token.

## Related

- [Accounts](accounts.md): creating, disabling and resetting the accounts an
  invitation or team membership names.
- [Email](email.md): SMTP configuration, the `email` doctor check and address
  verification.
- [Repositories](repositories.md): the repository roles a team grant carries.
