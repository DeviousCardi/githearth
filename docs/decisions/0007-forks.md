# ADR 0007: Fork contributions

Date: 2026-11-11.

Status: **accepted**.

## Context

Roadmap M3 ("Reviews and safe merges") calls for "fork contributions": a user
who can read a repository may fork it into a namespace they can write to, work
on the fork, and propose their changes back to the parent with a pull request
whose head lives in the fork. The merge then runs through the existing merge
path (merge intents, expected-target compare-and-swap, journal), not a second
engine.

Nothing for forks existed before this ADR: repositories had no parent
relationship, pull requests assumed both branches lived in one repository, and
the storage layer had no way to copy one repository's Git data into another.

## Decision

### Data model

- `repositories.parent_repository_id` records the fork relationship. `NULL`
  means "not a fork". The parent is only ever soft-deleted (`status =
  'deleted'`), so the foreign key needs no cascade.
- `pull_requests.source_repository_id` names where the head branch lives. For a
  same-repository pull it equals `repository_id`; for a fork pull it is the
  fork. The `(repository_id, source_repository_id, source_branch,
  target_branch)` uniqueness (one open pull per pair) and the "distinct branch
  names" check are rebuilt around it: only a same-repository pull must keep its
  two branch names distinct.
- `pull_requests.closed_reason` records why the system closed a pull (a fork
  being deleted or made inaccessible); a normal author/triage close leaves it
  `NULL`.

### Forking

A fork is created with `POST /api/v1/organizations/{org}/repositories/{repo}/forks`
naming the target organization (a namespace the actor can create repositories
in). The actor needs read on the parent and `repository.create` on the target
namespace. The fork inherits the parent's visibility, except that a private
parent always yields a private fork.

**Git data is copied as a full copy**, not alternates. The existing storage
layer's `create` produces a fresh, configured bare repository, then a new
`Storage::fetch_refs` fetches `+refs/heads/*` and `+refs/tags/*` (with all
reachable objects) from the parent. A full copy is chosen because:

- the storage layer has no alternates support, and alternates would couple the
  fork's object store to the parent's — a parent `git gc` or deletion would
  corrupt the fork;
- a full copy is independent, matches how the storage layer already isolates
  repositories, and keeps the fork's own journal (push history) starting empty.

The fork keeps its own configuration, hooks and empty push journal.

### Cross-repository pull requests

A pull request may name a fork as its source (`"source_repository":
"{org}/{repo}"` in the create body, absent for a same-repository pull). The
source must be an active fork of the target, and the actor must be able to read
it.

The merge brings the fork's objects into the target through the existing merge
path. `execute_intent` already holds the target's per-repository lock; for a
fork pull it additionally reads the head from the fork and, after revalidating
that the head still equals the recorded source oid, runs `Storage::fetch_ref`
to pull that branch's objects into the target. The candidate is then built and
the target ref moved exactly as for a same-repository pull: `build_candidate`,
`update_ref_expected`, journaling and `record_merge_result` are unchanged.

A push to the fork's head branch updates the pull's head through the same
"stale approvals" mechanism as same-repository pulls: reviews record the commit
they were given on, and only reviews at the current head count.

### Untrusted contributor policy

Fork authors get **no** grant on the target. The existing Cedar policies already
implement the policy without special cases:

- `pull_request.create` and `pull_request.comment` are in the `read` group, so
  a fork author who can read the parent may open and comment on a pull request.
- `pull_request.merge` and `ref.push` are in the `write` group, so only target
  members with merge permission can merge, and a fork author cannot push to the
  target (their Basic/SSH credential has no grant there).
- `pull_request.approve` is in the `write` group and the built-in
  `no-self-approval` forbid refuses the pull request author's own approval.

Forking therefore records only the `repository.forked` audit event and the
fork's row; it never writes a grant on the parent.

### Deleting or making a fork inaccessible

`repositories::finish_delete` closes every open pull request whose
`source_repository_id` is the deleted fork, recording
`closed_reason = 'source repository deleted'`. This runs on both the API delete
and the reconciler's delete path, so a fork that becomes inaccessible either way
cannot leave its pull requests dangling on an unreadable head.

## Consequences

- Fork creation performs a full copy; for very large repositories this is
  proportional to the parent's object store. Alternates can be revisited when
  measured storage pressure warrants it (and only with GC safeguards).
- A fork pull's merge fetches the fork's head objects into the target before
  building the candidate; the target's object store grows by the fetched
  history, which the target's own GC must retain while the pull exists.
- `merge_intents` needs no new column: the source repository is derived from the
  pull request (`source_repository_id`), and `execute_intent` revalidates it
  under the target lock.
