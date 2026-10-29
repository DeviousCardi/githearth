// Built-in policies, part of every decision. Forbids here cannot be
// overridden by any role, built-in or custom. Each policy has an @id that
// decisions report and tests name.

// --- Product permits ---------------------------------------------------

@id("public-read")
permit (principal, action in GitHearth::Action::"public_read", resource)
when {
  (resource is GitHearth::Repository && resource.visibility == "public") ||
  (resource has repository && resource.repository.visibility == "public")
};

@id("internal-read")
permit (principal, action in GitHearth::Action::"public_read", resource)
when {
  principal is GitHearth::Account &&
  ((resource is GitHearth::Repository &&
     resource.visibility == "internal" &&
     principal in resource.organization) ||
   (resource has repository &&
     resource.repository.visibility == "internal" &&
     principal in resource.organization))
};

@id("site-admin")
permit (principal, action in GitHearth::Action::"site_admin", resource)
when { principal is GitHearth::Account && principal.site_admin };

// --- Boundaries --------------------------------------------------------

// Instance administration belongs to site administrators alone, whatever
// an organization's roles say.
@id("site-admin-only")
forbid (principal, action in GitHearth::Action::"site_admin", resource)
unless { principal is GitHearth::Account && principal.site_admin };

@id("suspended-account")
forbid (principal, action, resource)
when { principal is GitHearth::Account && principal.suspended };

// Unauthenticated visitors get public reading and nothing else, whatever
// any role or misconfiguration permits.
@id("anonymous-public-read-only")
forbid (principal is GitHearth::Anonymous, action, resource)
unless { action in GitHearth::Action::"public_read" };

// Sudo mode: changing access, administering and destroying require an
// authentication fresher than the engine's window.
@id("sensitive-requires-fresh-auth")
forbid (principal, action in GitHearth::Action::"sensitive", resource)
when { context.auth_age_seconds > context.fresh_auth_seconds };

@id("suspended-organization")
forbid (principal, action, resource)
when {
  (resource is GitHearth::Organization && resource.suspended) ||
  (resource has organization && resource.organization.suspended)
}
unless { action in GitHearth::Action::"site_admin" };

@id("archived-read-only")
forbid (
  principal,
  action in [
    GitHearth::Action::"triage",
    GitHearth::Action::"write",
    GitHearth::Action::"maintain"
  ],
  resource
)
when {
  (resource is GitHearth::Repository && resource.archived) ||
  (resource has repository && resource.repository.archived)
};

// Issue and pull request creation are read-group actions, yet still writes.
@id("archived-no-new-content")
forbid (
  principal,
  action in [
    GitHearth::Action::"issue.create",
    GitHearth::Action::"issue.comment",
    GitHearth::Action::"pull_request.create",
    GitHearth::Action::"pull_request.comment"
  ],
  resource
)
when {
  (resource is GitHearth::Repository && resource.archived) ||
  (resource has repository && resource.repository.archived)
};

// --- Branch protection -------------------------------------------------

@id("protected-no-force-push")
forbid (principal, action == GitHearth::Action::"ref.force_push", resource)
when { resource.protected && !resource.allow_force_push };

@id("protected-no-deletion")
forbid (principal, action == GitHearth::Action::"ref.delete", resource)
when { resource.protected && !resource.allow_deletion };

@id("protected-requires-pull-request")
forbid (
  principal,
  action in [GitHearth::Action::"ref.push", GitHearth::Action::"ref.force_push"],
  resource
)
when { resource.protected && resource.require_pull_request && resource.exists };

@id("merge-requires-approvals")
forbid (principal, action == GitHearth::Action::"pull_request.merge", resource)
when {
  resource.target.protected &&
  context.approvals < resource.target.required_approvals
};

@id("merge-requires-checks")
forbid (principal, action == GitHearth::Action::"pull_request.merge", resource)
when {
  resource.target.protected &&
  resource.target.require_checks &&
  !context.checks_passed
};

@id("no-self-approval")
forbid (principal, action == GitHearth::Action::"pull_request.approve", resource)
when { resource.author == principal };
