// Built-in role "read". Linked per grant: ?principal is the account or team,
// ?resource the organization or repository the grant covers.
// `member` is deliberately excluded here: a read-only grant does not carry
// the base member privilege to create repositories (see the fork tests and
// issue #133, which is about the higher grants losing it).
permit (
  principal in ?principal,
  action in [GitHearth::Action::"read"],
  resource in ?resource
);
