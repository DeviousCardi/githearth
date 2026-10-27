//! Expected decisions for the built-in roles and policies, the hostile
//! custom-role check, and fail-closed behavior.

use githearth_authz::{
    Account, Action, BuiltinRole, Decision, Engine, Grant, Issue, Merge, Organization, Protection,
    PullRequest, Ref, Repository, Request, Resource, Role, Scope, Subject, Team, Visibility,
};
use uuid::Uuid;

/// Who, doing what, in which organization, to what, with which merge facts.
type Case<'a> = (
    &'a Account,
    Action,
    &'a Organization,
    Resource<'a>,
    Option<Merge>,
);

struct World {
    acme: Organization,
    other: Organization,
    eng: Team,
    devs: Team,
    private: Repository,
    public: Repository,
    internal: Repository,
    archived: Repository,
    foreign: Repository,
    main: Ref,
    feature: Ref,
    issue: Issue,
    pr: PullRequest,
    grants: Vec<Grant>,
    owner: Account,
    member: Account,
    developer: Account,
    collaborator: Account,
    outsider: Account,
    site_admin: Account,
    suspended: Account,
}

fn account(organizations: &[Uuid], teams: &[Uuid]) -> Account {
    Account {
        id: Uuid::new_v4(),
        suspended: false,
        site_admin: false,
        organizations: organizations.to_vec(),
        teams: teams.to_vec(),
    }
}

fn repository(organization: Uuid, visibility: Visibility, archived: bool) -> Repository {
    Repository {
        id: Uuid::new_v4(),
        organization,
        visibility,
        archived,
    }
}

fn grant(role: BuiltinRole, subject: Subject, scope: Scope) -> Grant {
    Grant {
        role: Role::Builtin(role),
        subject,
        scope,
    }
}

impl World {
    fn new() -> Self {
        let acme = Organization {
            id: Uuid::new_v4(),
            suspended: false,
        };
        let other = Organization {
            id: Uuid::new_v4(),
            suspended: false,
        };
        let eng = Team {
            id: Uuid::new_v4(),
            organization: acme.id,
            parent: None,
        };
        let devs = Team {
            id: Uuid::new_v4(),
            organization: acme.id,
            parent: Some(eng.id),
        };
        let private = repository(acme.id, Visibility::Private, false);
        let public = repository(acme.id, Visibility::Public, false);
        let internal = repository(acme.id, Visibility::Internal, false);
        let archived = repository(acme.id, Visibility::Private, true);
        let foreign = repository(other.id, Visibility::Private, false);
        let main = Ref {
            repository: private.id,
            name: "refs/heads/main".into(),
            protection: Some(Protection {
                require_pull_request: true,
                required_approvals: 2,
                require_checks: true,
                ..Protection::default()
            }),
        };
        let feature = Ref {
            repository: private.id,
            name: "refs/heads/feature".into(),
            protection: None,
        };
        let owner = account(&[acme.id], &[]);
        let member = account(&[acme.id], &[]);
        // Write on the private repository through `eng`, inherited by `devs`.
        let developer = account(&[acme.id], &[devs.id]);
        let collaborator = account(&[], &[]);
        let outsider = account(&[other.id], &[]);
        let site_admin = Account {
            site_admin: true,
            ..account(&[], &[])
        };
        let suspended = Account {
            suspended: true,
            ..account(&[acme.id], &[])
        };
        let issue = Issue {
            id: Uuid::new_v4(),
            repository: private.id,
            author: member.id,
        };
        let pr = PullRequest {
            id: Uuid::new_v4(),
            repository: private.id,
            author: developer.id,
            target: main.clone(),
        };
        let org = Scope::Organization(acme.id);
        let grants = vec![
            grant(BuiltinRole::Owner, Subject::Account(owner.id), org),
            grant(BuiltinRole::Member, Subject::Account(member.id), org),
            grant(BuiltinRole::Member, Subject::Account(developer.id), org),
            grant(BuiltinRole::Member, Subject::Account(suspended.id), org),
            grant(BuiltinRole::Admin, Subject::Account(suspended.id), org),
            grant(
                BuiltinRole::Write,
                Subject::Team(eng.id),
                Scope::Repository(private.id),
            ),
            grant(
                BuiltinRole::Read,
                Subject::Account(collaborator.id),
                Scope::Repository(private.id),
            ),
            grant(
                BuiltinRole::Owner,
                Subject::Account(outsider.id),
                Scope::Organization(other.id),
            ),
        ];
        Self {
            acme,
            other,
            eng,
            devs,
            private,
            public,
            internal,
            archived,
            foreign,
            main,
            feature,
            issue,
            pr,
            grants,
            owner,
            member,
            developer,
            collaborator,
            outsider,
            site_admin,
            suspended,
        }
    }

    fn teams(&self) -> Vec<Team> {
        vec![self.eng.clone(), self.devs.clone()]
    }

    fn decide(
        &self,
        engine: &Engine,
        who: &Account,
        action: Action,
        org: &Organization,
        resource: Resource<'_>,
        merge: Option<Merge>,
    ) -> Decision {
        let teams = self.teams();
        let teams: Vec<Team> = teams
            .into_iter()
            .filter(|t| who.organizations.contains(&t.organization))
            .collect();
        engine.authorize(&Request {
            principal: who,
            teams: &teams,
            grants: &self.grants,
            action,
            organization: org,
            resource,
            merge,
        })
    }
}

#[test]
fn decision_table() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let approved = Some(Merge {
        approvals: 2,
        checks_passed: true,
    });
    let unapproved = Some(Merge {
        approvals: 1,
        checks_passed: true,
    });
    let failing = Some(Merge {
        approvals: 2,
        checks_passed: false,
    });
    use Action::*;
    #[rustfmt::skip]
    let cases: Vec<(&str, Case<'_>, bool)> = vec![
        // Organization membership
        ("owner reads org", (&w.owner, OrganizationRead, &w.acme, Resource::Organization, None), true),
        ("owner deletes org", (&w.owner, OrganizationDelete, &w.acme, Resource::Organization, None), true),
        ("member reads org", (&w.member, OrganizationRead, &w.acme, Resource::Organization, None), true),
        ("member creates repo", (&w.member, RepositoryCreate, &w.acme, Resource::Organization, None), true),
        ("member cannot manage members", (&w.member, OrganizationManageMembers, &w.acme, Resource::Organization, None), false),
        ("outsider cannot read org", (&w.outsider, OrganizationRead, &w.acme, Resource::Organization, None), false),
        ("collaborator cannot read org", (&w.collaborator, OrganizationRead, &w.acme, Resource::Organization, None), false),
        ("owner role does not cross orgs", (&w.outsider, RepositoryRead, &w.acme, Resource::Repository(&w.private), None), false),
        ("owner of other org reads own repo", (&w.outsider, RepositoryRead, &w.other, Resource::Repository(&w.foreign), None), true),
        ("acme owner cannot read other org", (&w.owner, RepositoryRead, &w.other, Resource::Repository(&w.foreign), None), false),
        // Visibility
        ("anyone reads public repo", (&w.outsider, RepositoryClone, &w.acme, Resource::Repository(&w.public), None), true),
        ("public read is not write", (&w.outsider, IssueCreate, &w.acme, Resource::Repository(&w.public), None), false),
        ("member reads internal repo", (&w.member, RepositoryRead, &w.acme, Resource::Repository(&w.internal), None), true),
        ("outsider cannot read internal repo", (&w.outsider, RepositoryRead, &w.acme, Resource::Repository(&w.internal), None), false),
        ("member cannot read private repo", (&w.member, RepositoryRead, &w.acme, Resource::Repository(&w.private), None), false),
        ("owner reads private repo", (&w.owner, RepositoryRead, &w.acme, Resource::Repository(&w.private), None), true),
        ("collaborator reads private repo", (&w.collaborator, RepositoryClone, &w.acme, Resource::Repository(&w.private), None), true),
        ("collaborator cannot push", (&w.collaborator, RefPush, &w.acme, Resource::Ref(&w.private, &w.feature), None), false),
        // Nested teams
        ("nested team member pushes feature", (&w.developer, RefPush, &w.acme, Resource::Ref(&w.private, &w.feature), None), true),
        ("nested team member labels issue", (&w.developer, IssueLabel, &w.acme, Resource::Issue(&w.private, &w.issue), None), true),
        ("write cannot change settings", (&w.developer, RepositoryUpdate, &w.acme, Resource::Repository(&w.private), None), false),
        ("write cannot read other repos", (&w.developer, RepositoryRead, &w.acme, Resource::Repository(&w.archived), None), false),
        // Branch protection
        ("unprotected force push", (&w.developer, RefForcePush, &w.acme, Resource::Ref(&w.private, &w.feature), None), true),
        ("protected requires pull request", (&w.developer, RefPush, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("owner bound by protection too", (&w.owner, RefPush, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("protected no force push", (&w.owner, RefForcePush, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("protected no deletion", (&w.owner, RefDelete, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("merge with approvals and checks", (&w.owner, PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), approved), true),
        ("merge short of approvals", (&w.owner, PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), unapproved), false),
        ("merge with failing checks", (&w.owner, PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), failing), false),
        ("author cannot approve own PR", (&w.developer, PullRequestApprove, &w.acme, Resource::PullRequest(&w.private, &w.pr), None), false),
        ("other writer approves", (&w.owner, PullRequestApprove, &w.acme, Resource::PullRequest(&w.private, &w.pr), None), true),
        // Archived repositories
        ("archived is readable", (&w.owner, RepositoryRead, &w.acme, Resource::Repository(&w.archived), None), true),
        ("archived refuses new issues", (&w.owner, IssueCreate, &w.acme, Resource::Repository(&w.archived), None), false),
        ("archived refuses settings", (&w.owner, RepositoryUpdate, &w.acme, Resource::Repository(&w.archived), None), false),
        ("admin unarchives", (&w.owner, RepositoryUnarchive, &w.acme, Resource::Repository(&w.archived), None), true),
        // Accounts and site administration
        ("suspended account denied", (&w.suspended, OrganizationRead, &w.acme, Resource::Organization, None), false),
        ("site admin suspends org", (&w.site_admin, OrganizationSuspend, &w.acme, Resource::Organization, None), true),
        ("site admin cannot read private repo", (&w.site_admin, RepositoryRead, &w.acme, Resource::Repository(&w.private), None), false),
        ("owner cannot suspend org", (&w.owner, OrganizationSuspend, &w.acme, Resource::Organization, None), false),
    ];
    let mut failures = Vec::new();
    for (name, (who, action, org, resource, merge), expected) in cases {
        let decision = w.decide(&engine, who, action, org, resource, merge);
        if decision.is_allowed() != expected || matches!(decision, Decision::Invalid(_)) {
            failures.push(format!(
                "{name}: expected allow={expected}, got {decision:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn suspended_organization_blocks_everyone_but_site_admins() {
    let engine = Engine::new().unwrap();
    let mut w = World::new();
    w.acme.suspended = true;
    let acme = w.acme.clone();
    for action in [Action::OrganizationRead, Action::OrganizationUpdate] {
        let d = w.decide(
            &engine,
            &w.owner,
            action,
            &acme,
            Resource::Organization,
            None,
        );
        assert!(!d.is_allowed(), "{action:?}: {d:?}");
    }
    let d = w.decide(
        &engine,
        &w.owner,
        Action::RepositoryRead,
        &acme,
        Resource::Repository(&w.public),
        None,
    );
    assert_eq!(d, Decision::Deny(vec!["suspended-organization".into()]));
    let d = w.decide(
        &engine,
        &w.site_admin,
        Action::OrganizationUnsuspend,
        &acme,
        Resource::Organization,
        None,
    );
    assert!(d.is_allowed(), "{d:?}");
}

/// An organization role that permits every action on everything it scopes
/// must still lose to each built-in forbid, and must not leak across
/// organizations.
#[test]
fn hostile_custom_role_cannot_override_built_in_forbids() {
    let mut engine = Engine::new().unwrap();
    engine
        .add_custom_role(
            "everything",
            "permit (principal in ?principal, action, resource in ?resource);",
        )
        .unwrap();
    let mut w = World::new();
    for who in [w.developer.id, w.suspended.id, w.member.id] {
        w.grants.push(Grant {
            role: Role::Custom("everything".into()),
            subject: Subject::Account(who),
            scope: Scope::Organization(w.acme.id),
        });
    }
    let unapproved = Some(Merge {
        approvals: 0,
        checks_passed: false,
    });
    use Action::*;
    #[rustfmt::skip]
    let forbidden: Vec<(Case<'_>, &str)> = vec![
        ((&w.suspended, OrganizationRead, &w.acme, Resource::Organization, None), "suspended-account"),
        ((&w.developer, IssueCreate, &w.acme, Resource::Repository(&w.archived), None), "archived-no-new-content"),
        ((&w.developer, RepositoryUpdate, &w.acme, Resource::Repository(&w.archived), None), "archived-read-only"),
        ((&w.developer, RefForcePush, &w.acme, Resource::Ref(&w.private, &w.main), None), "protected-no-force-push"),
        ((&w.developer, RefDelete, &w.acme, Resource::Ref(&w.private, &w.main), None), "protected-no-deletion"),
        ((&w.developer, RefPush, &w.acme, Resource::Ref(&w.private, &w.main), None), "protected-requires-pull-request"),
        ((&w.developer, PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), unapproved), "merge-requires-approvals"),
        ((&w.developer, PullRequestApprove, &w.acme, Resource::PullRequest(&w.private, &w.pr), None), "no-self-approval"),
        ((&w.member, OrganizationSuspend, &w.acme, Resource::Organization, None), "site-admin-only"),
        ((&w.member, RepositoryRead, &w.other, Resource::Repository(&w.foreign), None), ""),
    ];
    for ((who, action, org, resource, merge), forbid) in forbidden {
        let d = w.decide(&engine, who, action, org, resource, merge);
        match d {
            Decision::Deny(ids) if forbid.is_empty() || ids.iter().any(|id| id == forbid) => {}
            other => panic!("{action:?}: expected deny by {forbid:?}, got {other:?}"),
        }
    }
    // The role does work where nothing forbids it.
    let d = w.decide(
        &engine,
        &w.member,
        RepositoryDelete,
        &w.acme,
        Resource::Repository(&w.private),
        None,
    );
    assert!(d.is_allowed(), "{d:?}");
}

#[test]
fn inconsistent_or_incomplete_facts_fail_closed() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let approved = Some(Merge {
        approvals: 9,
        checks_passed: true,
    });
    let invalid = |d: Decision| assert!(matches!(d, Decision::Invalid(_)), "{d:?}");
    // Repository claimed to be in the wrong organization.
    invalid(w.decide(
        &engine,
        &w.outsider,
        Action::RepositoryRead,
        &w.other,
        Resource::Repository(&w.private),
        None,
    ));
    // Action applied to the wrong resource kind.
    invalid(w.decide(
        &engine,
        &w.owner,
        Action::RepositoryRead,
        &w.acme,
        Resource::Organization,
        None,
    ));
    // Merge facts missing, or given for a non-merge action.
    invalid(w.decide(
        &engine,
        &w.owner,
        Action::PullRequestMerge,
        &w.acme,
        Resource::PullRequest(&w.private, &w.pr),
        None,
    ));
    invalid(w.decide(
        &engine,
        &w.owner,
        Action::PullRequestRead,
        &w.acme,
        Resource::PullRequest(&w.private, &w.pr),
        approved,
    ));
    // A ref from another repository.
    let stray = Ref {
        repository: w.public.id,
        ..w.feature.clone()
    };
    invalid(w.decide(
        &engine,
        &w.owner,
        Action::RefPush,
        &w.acme,
        Resource::Ref(&w.private, &stray),
        None,
    ));
    // A principal team whose facts were not loaded.
    invalid(engine.authorize(&Request {
        principal: &w.developer,
        teams: &[],
        grants: &w.grants,
        action: Action::RefPush,
        organization: &w.acme,
        resource: Resource::Ref(&w.private, &w.feature),
        merge: None,
    }));
    // A team whose parent was not loaded.
    invalid(engine.authorize(&Request {
        principal: &w.developer,
        teams: std::slice::from_ref(&w.devs),
        grants: &w.grants,
        action: Action::RefPush,
        organization: &w.acme,
        resource: Resource::Ref(&w.private, &w.feature),
        merge: None,
    }));
}

#[test]
fn unusable_grants_grant_nothing() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let member = account(&[], &[]);
    let grants = [
        // Membership roles are organization-only.
        grant(
            BuiltinRole::Owner,
            Subject::Account(member.id),
            Scope::Repository(w.private.id),
        ),
        // Unknown custom role.
        Grant {
            role: Role::Custom("missing".into()),
            subject: Subject::Account(member.id),
            scope: Scope::Organization(w.acme.id),
        },
    ];
    let d = engine.authorize(&Request {
        principal: &member,
        teams: &[],
        grants: &grants,
        action: Action::RepositoryRead,
        organization: &w.acme,
        resource: Resource::Repository(&w.private),
        merge: None,
    });
    assert_eq!(d, Decision::Deny(vec![]));
}
