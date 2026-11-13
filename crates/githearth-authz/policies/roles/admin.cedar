// Built-in role "admin". Linked per grant: ?principal is the account or team,
// ?resource the organization or repository the grant covers.
permit (
  principal in ?principal,
  action in [GitHearth::Action::"member", GitHearth::Action::"read", GitHearth::Action::"triage", GitHearth::Action::"write", GitHearth::Action::"maintain", GitHearth::Action::"admin"],
  resource in ?resource
);
