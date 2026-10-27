// Built-in role "write". Linked per grant: ?principal is the account or team,
// ?resource the organization or repository the grant covers.
permit (
  principal in ?principal,
  action in [GitHearth::Action::"read", GitHearth::Action::"triage", GitHearth::Action::"write"],
  resource in ?resource
);
