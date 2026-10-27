// Built-in role "owner". Linked per grant: ?principal is the account or team,
// ?resource the organization or repository the grant covers.
permit (
  principal in ?principal,
  action in [GitHearth::Action::"member", GitHearth::Action::"read", GitHearth::Action::"triage", GitHearth::Action::"write", GitHearth::Action::"maintain", GitHearth::Action::"admin", GitHearth::Action::"owner"],
  resource in ?resource
);
