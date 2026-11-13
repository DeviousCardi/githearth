# ADR 0002: Policy-based authorization with Cedar

Date: 2026-10-27.

Status: **accepted for implementation**. The engine, schema, built-in policies and role templates exist in `crates/githearth-authz`; the organization read path uses it. Persisted teams, grants and branch protection arrive with their M2/M3 tables.

## Decision

Every permission check goes through one engine, `githearth_authz::Engine::authorize`, which evaluates [Cedar](https://www.cedarpolicy.com/) policies (`cedar-policy`, Apache-2.0) against entities built from authoritative facts. No handler compares role strings or writes its own permission logic.

A decision combines:

1. **Built-in policies** (`policies/builtin.cedar`): product permits (public and internal visibility, site administration) and forbids that no role can override.
2. **Role templates** (`policies/roles/*.cedar`): one `permit (principal in ?principal, action in [groups…], resource in ?resource)` per role, linked once per grant. A grant names a role, a subject (account or team) and a scope (organization or repository).
3. **Entities** built by the engine from facts the service loaded: the account with its organizations and teams as parents, nested teams, the organization, and the repository/ref/issue/pull request involved. Callers never pass Cedar entities or context.

The schema (`policies/githearth.cedarschema`) is the contract. Strict validation runs on startup and on every custom role.

## Model

Hierarchy: `Account in [Team, Organization]`, `Team in [Team, Organization]`, `Repository in Organization`, and `Ref`, `Issue` and `PullRequest` in `Repository`. A grant on an organization reaches its repositories, and a grant to a team reaches the members of that team and of its descendant teams.

Actions are named `object.verb` and grouped. Templates name only groups, so a new action joins a group in the schema and no template changes.

| Group | Examples |
| --- | --- |
| `public_read` | `repository.read`, `repository.clone`, `issue.read`, `pull_request.read` |
| `member` | `organization.read`, `repository.create` |
| `read` | `public_read` actions plus `issue.create`, `issue.comment`, `pull_request.create` |
| `triage` | `issue.label`, `issue.assign`, `issue.close`, `pull_request.close` |
| `write` | `ref.push`, `ref.force_push`, `ref.delete`, `pull_request.approve`, `pull_request.merge` |
| `maintain` | `repository.update` (branch and tag protection mutations) |
| `admin` | `repository.manage_access`, `archive`, `unarchive`, `delete` |
| `owner` | `organization.update`, `manage_members`, `delete`, `team.create` |
| `site_admin` | `organization.suspend`, `organization.unsuspend` |
| `sensitive` | sudo mode: an action here also sits in its functional group above — `organization.update`, `manage_members`, `delete`, `organization.suspend`, `unsuspend`, `team.create`, `repository.manage_access`, `archive`, `unarchive`, `delete`, `ref.force_push`, `ref.delete` |

Built-in roles: `member` and `owner` (organization scope only), plus `read`, `triage`, `write`, `maintain` and `admin` (organization or repository scope). Each role includes the groups below it. The M1 membership values `owner`/`member` map onto the first two.

Branch and tag protection are two halves of one feature and share a single gate: creating, editing and deleting either kind of rule is `repository.update`, so a `maintain` member can manage both. Tag protection was briefly gated on a separate `repository.manage_protection` action in the `admin` group; that action was removed (see #135) because it split the feature across two roles.

The principal of every action is a signed-in `Account` or the unauthenticated `Anonymous`, which has no attributes, no parents and may carry no grants or teams.

## Built-in forbids

These hold whatever any role, built-in or custom, permits:

- `site-admin-only`: instance administration requires a site administrator.
- `suspended-account`: a suspended account can do nothing.
- `suspended-organization`: nothing happens inside a suspended organization except site administration.
- `anonymous-public-read-only`: an anonymous visitor only reaches the `public_read` group, so no misconfiguration or custom role can let it write.
- `sensitive-requires-fresh-auth`: sensitive actions are forbidden when the time since the account last presented its password exceeds the engine's fresh-auth window (`DEFAULT_FRESH_AUTH`, 15 minutes; configurable with `Engine::with_fresh_auth`). Unknown recency counts as stale. Non-sensitive actions never require it.
- `archived-read-only`, `archived-no-new-content`: archived repositories accept no triage, write, maintain, issues, pull requests or comments. Admin actions such as unarchive remain.
- `protected-no-force-push`, `protected-no-deletion`, `protected-requires-pull-request`: branch protection binds owners and admins too. There is no bypass role yet; adding one is a separate decision.
- `merge-requires-approvals`, `merge-requires-checks`: merge context (`approvals`, `checks_passed`) is computed by the service for the exact head and target revisions being merged, so a stale approval or check cannot authorize different code (roadmap M3).
- `no-self-approval`: a pull request's author cannot approve it.

The engine fills every action's context (`auth_age_seconds`, `fresh_auth_seconds`, and merge facts for merges); callers never build context.

`tests/decisions.rs` holds the expected decision table. Its hostile test grants a custom permit-everything role and proves every forbid above still denies and that the role does not cross organizations.

## Failing closed

The engine denies with `Decision::Invalid` when facts are inconsistent (a repository in another organization, a ref from another repository, a team or parent team not loaded, merge facts missing or misplaced) or when Cedar reports any evaluation error. Cedar alone skips a policy that errors, which would turn a broken forbid into an allow. A grant the engine cannot use (unknown custom role, membership role on a repository) grants nothing instead of failing the whole decision.

Services load the object first, and return 404 when it is missing *or* the decision denies, so a denial reveals nothing (`GET /api/v1/organizations/{id}` does this today).

## Listing and search

Cedar answers "may X do Y to Z", not "which Z may X see". List, search and dashboard queries keep a SQL filter derived from the same grants and visibility rules, then authorize each materialized result before returning it. A test for each list endpoint must show that the SQL filter never returns an object the engine would deny. This matches the roadmap rule that permissions apply at query time and at materialization.

## Custom roles and organization policy

`Engine::add_custom_role` accepts only a single `permit` template with both slots, validated against the schema. Custom roles are an M11 feature; the engine supports them now so the hostile test can prove the forbids. Organization-defined policy sets (M11) will be **forbid-only**, applying within the organization's own subtree, so an organization can restrict its members but never grant past the built-ins or bind site administrators.

## Open items

- **Revocation and caching.** Decisions currently load facts per request from PostgreSQL, so a committed revocation applies to the next admitted request. Any cache, or the M5 storage nodes authorizing pushes locally, needs a versioned change feed and a stated maximum staleness before it ships.
- **Performance.** Each decision clones the policy set and links one template per grant. Measure decision latency at the enterprise profile (large single organization, many teams and grants) before M4. If needed, cache linked sets per principal keyed by a grant version.
- **Persistence.** Tables for teams, team membership, grants and protection rules land with M2 repositories and M3 reviews. Grant writes validate the scope against the role's assignability, and that the subject belongs to the scope's organization.

## Toolchain

`cedar-policy` 4.14 requires Rust 1.89, so the workspace `rust-version` and the Dockerfile builder moved from 1.85 to 1.89.
