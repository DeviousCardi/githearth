# Repositories: deletion and restore

Deleting a repository moves it to trash. While the trash entry exists and the
retention window has not passed, an organization owner or a site administrator
can restore it from the API, the browser or a host command. The restore keeps
the repository ID, history, issues and pull requests.

## Delete

| Action | UI | API |
| --- | --- | --- |
| Delete a repository | "Delete" on the repository page | `DELETE /api/v1/organizations/{org}/repositories/{name}` |

Deleting needs a repository `admin` grant (or an owner/site-administrator
authority) and is a `sensitive` action: an API or browser session must have
authenticated within the last 15 minutes. The row moves `active → deleting →
deleted`, the directory moves into trash, and the operation writes one
`repository.deleted` audit event. The repository's grants are revoked in the
same step, so nobody keeps access through a stale grant; a restore does **not**
recreate a revoked grant.

A deleted path returns `410 Gone` to a caller that could have seen the
repository before (a reader of a public repository, or a grant holder) or could
restore it; anyone else gets the same `404` an unknown private name gives. A
live repository that reuses the name wins over the deleted row, so the old
path never resolves to someone else's repository.

The reconciler purges the trash entry after `trash_retention_seconds`
(`GITHEARTH_TRASH_RETENTION_SECONDS`, default `604800` — seven days; range one
hour to one year). The deleted row remains, but its bytes are gone, so a
restore after the purge is refused.

## Restore

| Action | UI | API |
| --- | --- | --- |
| Restore a deleted repository | form on the organization page | `POST /api/v1/organizations/{org}/repositories/{name}/restore` |

Restoring needs the organization **owner** grant or site-administrator
authority; a repository `admin` grant is not enough. The operation:

1. refuses when the repository is not deleted, when a live repository already
   uses the name (`409 repository name already taken`), or when the trash entry
   has been purged (`409 repository is no longer in trash`);
2. moves the row to `restoring`, moves the directory back from trash, then moves
   the row back to `active` and writes `repository.restored`;
3. keeps the original repository ID and `deleted_at` is cleared. History,
   issues, pull requests and the audit chain stay attached to the same row.

The move takes the per-repository filesystem lock that the reconciler and the
purge also take, so a purge that wins first leaves the row `deleted` and the
restore is refused rather than half-done. A crashed restore is completed or
reverted by the reconciler: the row is `restoring` until the storage move
finishes, and returns to `deleted` if the trash entry disappeared.

A restore is idempotent through the operation id: repeating the same request
returns the restored repository, while a second attempt after success reports
`409 repository is not deleted`.

## The CLI

```sh
githearth-server --config /etc/githearth/config.toml repository restore \
  --organization acme --name website --actor alice
```

`--actor` names an existing account: it must be enabled (not suspended and not
pending a password change) and must hold the organization owner grant or be a
site administrator. The command runs the same operation and authorization as
the API and the browser — there is no host-operator bypass. It prints
`restored acme/website (<id>) as alice` or `restore refused: <reason> (<status>)`.

**It needs no running server.** The invocation is treated as fresh
authentication: the command carries no session age, so the 15-minute
`sensitive` window that applies to the API and the browser does not apply to
this explicit host invocation. That is a host-tool choice, not new authority:
the named account still needs the owner or site-administrator authority, and
every refusal is the one the API gives. The command uses the database and the
storage root directly, taking the same per-repository filesystem lock as a
running server's reconciler and purge.

## Related

- [Backup and restore](backup-restore.md): restoring a whole installation,
  including repositories, from an encrypted archive.
- [Teams](teams.md): repository grants that a delete revokes.
- [Audit retention](audit-retention.md): the audit rows a restore keeps.
