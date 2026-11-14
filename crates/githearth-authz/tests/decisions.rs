//! Expected decisions for the built-in roles and policies, the hostile
//! custom-role check, anonymous visitors, fresh authentication, and
//! fail-closed behavior.

use std::time::Duration;

use githearth_authz::{
    Account, Action, BuiltinRole, Decision, Engine, Grant, Issue, Merge, Organization, Principal,
    Protection, PullRequest, Ref, Repository, Request, Resource, Role, Scope, Subject, Team,
    Visibility,
};
use uuid::Uuid;

/// Who, doing what, in which organization, to what, with which merge facts.
type Case<'a> = (
    Principal<'a>,
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
            exists: true,
        };
        let feature = Ref {
            repository: private.id,
            name: "refs/heads/feature".into(),
            protection: None,
            exists: true,
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
        engine.authorize(&Request {
            principal: Principal::Account(who),
            teams: &self.accountable_teams(who),
            grants: &self.grants,
            action,
            organization: org,
            resource,
            merge,
            auth_age: Some(Duration::ZERO),
            read_only: false,
        })
    }

    /// An unauthenticated visitor: never grants, never teams.
    fn anonymous(
        &self,
        engine: &Engine,
        action: Action,
        org: &Organization,
        resource: Resource<'_>,
        merge: Option<Merge>,
    ) -> Decision {
        engine.authorize(&Request {
            principal: Principal::Anonymous,
            teams: &[],
            grants: &[],
            action,
            organization: org,
            resource,
            merge,
            auth_age: None,
            read_only: false,
        })
    }

    fn accountable_teams(&self, who: &Account) -> Vec<Team> {
        self.teams()
            .into_iter()
            .filter(|t| who.organizations.contains(&t.organization))
            .collect()
    }
}

/// A repository to act on, with a ref, an issue and a pull request inside,
/// for tests that walk every action over one repository.
struct Site<'a> {
    repository: &'a Repository,
    reference: Ref,
    issue: Issue,
    pr: PullRequest,
}

impl Site<'_> {
    /// The resource `action` names here, with the merge facts a merge carries.
    fn resource(&self, action: Action) -> (Resource<'_>, Option<Merge>) {
        let merge = (action == Action::PullRequestMerge).then_some(Merge {
            approvals: 1,
            checks_passed: true,
            code_owner_review_ok: true,
        });
        use Action::*;
        let repository = self.repository;
        let resource = match action {
            OrganizationRead
            | OrganizationUpdate
            | OrganizationManageMembers
            | OrganizationDelete
            | OrganizationSuspend
            | OrganizationUnsuspend
            | TeamCreate
            | RepositoryCreate => Resource::Organization,
            AccountRead | AccountCreate | AccountDisable | AccountEnable | AccountResetPassword => {
                Resource::Instance
            }
            RepositoryRead
            | RepositoryClone
            | RepositoryUpdate
            | RepositoryManageAccess
            | RepositoryArchive
            | RepositoryUnarchive
            | RepositoryDelete
            | IssueCreate
            | PullRequestCreate => Resource::Repository(repository),
            RefPush | RefForcePush | RefDelete => Resource::Ref(repository, &self.reference),
            StatusCreate => Resource::Repository(repository),
            IssueRead | IssueComment | IssueLabel | IssueAssign | IssueClose => {
                Resource::Issue(repository, &self.issue)
            }
            PullRequestRead | PullRequestComment | PullRequestClose | PullRequestApprove
            | PullRequestMerge => Resource::PullRequest(repository, &self.pr),
        };
        (resource, merge)
    }
}

/// A site on the given repository, containing only facts the caller loads.
fn site_on<'a>(repository: &'a Repository, author: Uuid, branch: &str) -> Site<'a> {
    let reference = Ref {
        repository: repository.id,
        name: branch.into(),
        protection: None,
        exists: true,
    };
    let issue = Issue {
        id: Uuid::new_v4(),
        repository: repository.id,
        author,
    };
    let pr = PullRequest {
        id: Uuid::new_v4(),
        repository: repository.id,
        author,
        target: reference.clone(),
    };
    Site {
        repository,
        reference,
        issue,
        pr,
    }
}

/// The `public_read` actions.
fn is_public_read(action: Action) -> bool {
    matches!(
        action,
        Action::RepositoryRead
            | Action::RepositoryClone
            | Action::IssueRead
            | Action::PullRequestRead
    )
}

#[test]
fn decision_table() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let approved = Some(Merge {
        approvals: 2,
        checks_passed: true,
        code_owner_review_ok: true,
    });
    let unapproved = Some(Merge {
        approvals: 1,
        checks_passed: true,
        code_owner_review_ok: true,
    });
    let failing = Some(Merge {
        approvals: 2,
        checks_passed: false,
        code_owner_review_ok: true,
    });
    use Action::*;
    #[rustfmt::skip]
    let cases: Vec<(&str, Case<'_>, bool)> = vec![
        // Organization membership
        ("owner reads org", (Principal::Account(&w.owner), OrganizationRead, &w.acme, Resource::Organization, None), true),
        ("owner deletes org", (Principal::Account(&w.owner), OrganizationDelete, &w.acme, Resource::Organization, None), true),
        ("member reads org", (Principal::Account(&w.member), OrganizationRead, &w.acme, Resource::Organization, None), true),
        ("member creates repo", (Principal::Account(&w.member), RepositoryCreate, &w.acme, Resource::Organization, None), true),
        ("member cannot manage members", (Principal::Account(&w.member), OrganizationManageMembers, &w.acme, Resource::Organization, None), false),
        ("outsider cannot read org", (Principal::Account(&w.outsider), OrganizationRead, &w.acme, Resource::Organization, None), false),
        ("collaborator cannot read org", (Principal::Account(&w.collaborator), OrganizationRead, &w.acme, Resource::Organization, None), false),
        ("owner role does not cross orgs", (Principal::Account(&w.outsider), RepositoryRead, &w.acme, Resource::Repository(&w.private), None), false),
        ("owner of other org reads own repo", (Principal::Account(&w.outsider), RepositoryRead, &w.other, Resource::Repository(&w.foreign), None), true),
        ("acme owner cannot read other org", (Principal::Account(&w.owner), RepositoryRead, &w.other, Resource::Repository(&w.foreign), None), false),
        // Visibility
        ("anyone reads public repo", (Principal::Account(&w.outsider), RepositoryClone, &w.acme, Resource::Repository(&w.public), None), true),
        ("public read is not write", (Principal::Account(&w.outsider), IssueCreate, &w.acme, Resource::Repository(&w.public), None), false),
        ("member reads internal repo", (Principal::Account(&w.member), RepositoryRead, &w.acme, Resource::Repository(&w.internal), None), true),
        ("outsider cannot read internal repo", (Principal::Account(&w.outsider), RepositoryRead, &w.acme, Resource::Repository(&w.internal), None), false),
        ("member cannot read private repo", (Principal::Account(&w.member), RepositoryRead, &w.acme, Resource::Repository(&w.private), None), false),
        ("owner reads private repo", (Principal::Account(&w.owner), RepositoryRead, &w.acme, Resource::Repository(&w.private), None), true),
        ("collaborator reads private repo", (Principal::Account(&w.collaborator), RepositoryClone, &w.acme, Resource::Repository(&w.private), None), true),
        ("collaborator cannot push", (Principal::Account(&w.collaborator), RefPush, &w.acme, Resource::Ref(&w.private, &w.feature), None), false),
        // Nested teams
        ("nested team member pushes feature", (Principal::Account(&w.developer), RefPush, &w.acme, Resource::Ref(&w.private, &w.feature), None), true),
        ("nested team member labels issue", (Principal::Account(&w.developer), IssueLabel, &w.acme, Resource::Issue(&w.private, &w.issue), None), true),
        ("write cannot change settings", (Principal::Account(&w.developer), RepositoryUpdate, &w.acme, Resource::Repository(&w.private), None), false),
        ("write cannot read other repos", (Principal::Account(&w.developer), RepositoryRead, &w.acme, Resource::Repository(&w.archived), None), false),
        // Branch protection
        ("unprotected force push", (Principal::Account(&w.developer), RefForcePush, &w.acme, Resource::Ref(&w.private, &w.feature), None), true),
        ("protected requires pull request", (Principal::Account(&w.developer), RefPush, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("owner bound by protection too", (Principal::Account(&w.owner), RefPush, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("protected no force push", (Principal::Account(&w.owner), RefForcePush, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("protected no deletion", (Principal::Account(&w.owner), RefDelete, &w.acme, Resource::Ref(&w.private, &w.main), None), false),
        ("merge with approvals and checks", (Principal::Account(&w.owner), PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), approved), true),
        ("merge short of approvals", (Principal::Account(&w.owner), PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), unapproved), false),
        ("merge with failing checks", (Principal::Account(&w.owner), PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), failing), false),
        ("author cannot approve own PR", (Principal::Account(&w.developer), PullRequestApprove, &w.acme, Resource::PullRequest(&w.private, &w.pr), None), false),
        ("other writer approves", (Principal::Account(&w.owner), PullRequestApprove, &w.acme, Resource::PullRequest(&w.private, &w.pr), None), true),
        // Archived repositories
        ("archived is readable", (Principal::Account(&w.owner), RepositoryRead, &w.acme, Resource::Repository(&w.archived), None), true),
        ("archived refuses new issues", (Principal::Account(&w.owner), IssueCreate, &w.acme, Resource::Repository(&w.archived), None), false),
        ("archived refuses settings", (Principal::Account(&w.owner), RepositoryUpdate, &w.acme, Resource::Repository(&w.archived), None), false),
        ("admin unarchives", (Principal::Account(&w.owner), RepositoryUnarchive, &w.acme, Resource::Repository(&w.archived), None), true),
        // Accounts and site administration
        ("suspended account denied", (Principal::Account(&w.suspended), OrganizationRead, &w.acme, Resource::Organization, None), false),
        ("site admin suspends org", (Principal::Account(&w.site_admin), OrganizationSuspend, &w.acme, Resource::Organization, None), true),
        ("site admin cannot read private repo", (Principal::Account(&w.site_admin), RepositoryRead, &w.acme, Resource::Repository(&w.private), None), false),
        ("owner cannot suspend org", (Principal::Account(&w.owner), OrganizationSuspend, &w.acme, Resource::Organization, None), false),
    ];
    let mut failures = Vec::new();
    for (name, (who, action, org, resource, merge), expected) in cases {
        let decision = match who {
            Principal::Account(who) => w.decide(&engine, who, action, org, resource, merge),
            Principal::Anonymous => w.anonymous(&engine, action, org, resource, merge),
        };
        if decision.is_allowed() != expected || matches!(decision, Decision::Invalid(_)) {
            failures.push(format!(
                "{name}: expected allow={expected}, got {decision:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn code_owner_review_requirement_blocks_merge_until_satisfied() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let protected = Ref {
        protection: Some(Protection {
            require_code_owner_review: true,
            ..Protection::default()
        }),
        ..w.main.clone()
    };
    let pr = PullRequest {
        target: protected.clone(),
        ..w.pr.clone()
    };
    let ok = Some(Merge {
        approvals: 0,
        checks_passed: true,
        code_owner_review_ok: true,
    });
    let missing = Some(Merge {
        approvals: 0,
        checks_passed: true,
        code_owner_review_ok: false,
    });

    let satisfied = w.decide(
        &engine,
        &w.owner,
        Action::PullRequestMerge,
        &w.acme,
        Resource::PullRequest(&w.private, &pr),
        ok,
    );
    assert!(satisfied.is_allowed(), "{satisfied:?}");

    let refused = w.decide(
        &engine,
        &w.owner,
        Action::PullRequestMerge,
        &w.acme,
        Resource::PullRequest(&w.private, &pr),
        missing,
    );
    assert_eq!(
        refused,
        Decision::Deny(vec!["merge-requires-code-owner-review".into()])
    );
}

#[test]
fn branch_creation_is_allowed_under_require_pull_request() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    // Creating a ref that matches a protected glob (old oid all zeros) is a
    // plain ref.push on a ref that does not exist yet: allowed.
    let new_branch = Ref {
        exists: false,
        ..w.main.clone()
    };
    let d = w.decide(
        &engine,
        &w.developer,
        Action::RefPush,
        &w.acme,
        Resource::Ref(&w.private, &new_branch),
        None,
    );
    assert!(d.is_allowed(), "creation push must be allowed: {d:?}");

    // Once the ref exists, the require_pull_request rule applies and refuses
    // the same fast-forward push.
    let d = w.decide(
        &engine,
        &w.developer,
        Action::RefPush,
        &w.acme,
        Resource::Ref(&w.private, &w.main),
        None,
    );
    assert_eq!(
        d,
        Decision::Deny(vec!["protected-requires-pull-request".into()])
    );
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

/// Account administration is instance administration: a site administrator may
/// create, disable, enable and reset accounts; no organization role can, and a
/// disabled account can do nothing at all.
#[test]
fn account_administration_requires_a_site_admin() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let actions = [
        Action::AccountRead,
        Action::AccountCreate,
        Action::AccountDisable,
        Action::AccountEnable,
        Action::AccountResetPassword,
    ];
    for action in actions {
        let allowed = w.decide(
            &engine,
            &w.site_admin,
            action,
            &w.acme,
            Resource::Instance,
            None,
        );
        assert!(allowed.is_allowed(), "{action:?}: {allowed:?}");
        // An organization owner is not a site administrator.
        let denied = w.decide(&engine, &w.owner, action, &w.acme, Resource::Instance, None);
        assert!(
            matches!(&denied, Decision::Deny(ids) if ids.iter().any(|id| id == "site-admin-only")),
            "{action:?}: expected site-admin-only, got {denied:?}"
        );
    }
    // A disabled account has no active principal, even if it is a site admin.
    let disabled = Account {
        suspended: true,
        site_admin: true,
        ..account(&[], &[])
    };
    let denied = w.decide(
        &engine,
        &disabled,
        Action::AccountCreate,
        &w.acme,
        Resource::Instance,
        None,
    );
    assert!(
        matches!(&denied, Decision::Deny(ids) if ids.iter().any(|id| id == "suspended-account")),
        "expected suspended-account, got {denied:?}"
    );
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
        code_owner_review_ok: false,
    });
    use Action::*;
    #[rustfmt::skip]
    let forbidden: Vec<(Case<'_>, &str)> = vec![
        ((Principal::Account(&w.suspended), OrganizationRead, &w.acme, Resource::Organization, None), "suspended-account"),
        ((Principal::Account(&w.developer), IssueCreate, &w.acme, Resource::Repository(&w.archived), None), "archived-no-new-content"),
        ((Principal::Account(&w.developer), RepositoryUpdate, &w.acme, Resource::Repository(&w.archived), None), "archived-read-only"),
        ((Principal::Account(&w.developer), RefForcePush, &w.acme, Resource::Ref(&w.private, &w.main), None), "protected-no-force-push"),
        ((Principal::Account(&w.developer), RefDelete, &w.acme, Resource::Ref(&w.private, &w.main), None), "protected-no-deletion"),
        ((Principal::Account(&w.developer), RefPush, &w.acme, Resource::Ref(&w.private, &w.main), None), "protected-requires-pull-request"),
        ((Principal::Account(&w.developer), PullRequestMerge, &w.acme, Resource::PullRequest(&w.private, &w.pr), unapproved), "merge-requires-approvals"),
        ((Principal::Account(&w.developer), PullRequestApprove, &w.acme, Resource::PullRequest(&w.private, &w.pr), None), "no-self-approval"),
        ((Principal::Account(&w.member), OrganizationSuspend, &w.acme, Resource::Organization, None), "site-admin-only"),
        ((Principal::Account(&w.member), RepositoryRead, &w.other, Resource::Repository(&w.foreign), None), ""),
    ];
    for ((who, action, org, resource, merge), forbid) in forbidden {
        let d = match who {
            Principal::Account(who) => w.decide(&engine, who, action, org, resource, merge),
            Principal::Anonymous => w.anonymous(&engine, action, org, resource, merge),
        };
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
        code_owner_review_ok: true,
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
        principal: Principal::Account(&w.developer),
        teams: &[],
        grants: &w.grants,
        action: Action::RefPush,
        organization: &w.acme,
        resource: Resource::Ref(&w.private, &w.feature),
        merge: None,
        auth_age: Some(Duration::ZERO),
        read_only: false,
    }));
    // A team whose parent was not loaded.
    invalid(engine.authorize(&Request {
        principal: Principal::Account(&w.developer),
        teams: std::slice::from_ref(&w.devs),
        grants: &w.grants,
        action: Action::RefPush,
        organization: &w.acme,
        resource: Resource::Ref(&w.private, &w.feature),
        merge: None,
        auth_age: Some(Duration::ZERO),
        read_only: false,
    }));
    let push = |principal: &Account, teams: &[Team]| {
        engine.authorize(&Request {
            principal: Principal::Account(principal),
            teams,
            grants: &w.grants,
            action: Action::RefPush,
            organization: &w.acme,
            resource: Resource::Ref(&w.private, &w.feature),
            merge: None,
            auth_age: Some(Duration::ZERO),
            read_only: false,
        })
    };
    // The same team loaded twice.
    let mut doubled = w.teams();
    doubled.push(w.devs.clone());
    invalid(push(&w.developer, &doubled));
    // A principal team in an organization the principal does not belong to.
    let stranger = Account {
        organizations: vec![w.other.id],
        ..w.developer.clone()
    };
    invalid(push(&stranger, &w.teams()));
    // A parent team in another organization.
    let crossed: Vec<Team> = w
        .teams()
        .into_iter()
        .map(|team| match team.parent {
            None => Team {
                organization: w.other.id,
                ..team
            },
            Some(_) => team,
        })
        .collect();
    invalid(push(&w.developer, &crossed));
}

#[test]
fn engine_errors_are_reported() {
    let mut engine = Engine::new().unwrap();
    let huge = "x".repeat(githearth_authz::MAX_ROLE_BYTES + 1);
    let error = engine.add_custom_role("huge", &huge).unwrap_err();
    assert_eq!(error.to_string(), "custom role: template too large");
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
        principal: Principal::Account(&member),
        teams: &[],
        grants: &grants,
        action: Action::RepositoryRead,
        organization: &w.acme,
        resource: Resource::Repository(&w.private),
        merge: None,
        auth_age: Some(Duration::ZERO),
        read_only: false,
    });
    assert_eq!(d, Decision::Deny(vec![]));
}

/// Public repositories are readable and clonable without signing in; an
/// anonymous visitor can do nothing else, even where suspensions say yes.
#[test]
fn anonymous_reads_public_repositories_only() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let site = site_on(&w.public, w.member.id, "refs/heads/main");
    for action in [
        Action::RepositoryRead,
        Action::RepositoryClone,
        Action::IssueRead,
        Action::PullRequestRead,
    ] {
        let (resource, _) = site.resource(action);
        let d = w.anonymous(&engine, action, &w.acme, resource, None);
        assert!(d.is_allowed(), "{action:?}: {d:?}");
    }
    for repository in [&w.internal, &w.private] {
        let d = w.anonymous(
            &engine,
            Action::RepositoryRead,
            &w.acme,
            Resource::Repository(repository),
            None,
        );
        assert_eq!(d, Decision::Deny(vec![]), "{repository:?}: {d:?}");
    }
    for &action in Action::ALL {
        if is_public_read(action) {
            continue;
        }
        let (resource, merge) = site.resource(action);
        let d = w.anonymous(&engine, action, &w.acme, resource, merge);
        assert!(
            matches!(d, Decision::Deny(_)),
            "{action:?}: expected clean deny, got {d:?}"
        );
    }
}

#[test]
fn anonymous_reads_nothing_in_a_suspended_organization() {
    let engine = Engine::new().unwrap();
    let mut w = World::new();
    w.acme.suspended = true;
    let d = w.anonymous(
        &engine,
        Action::RepositoryRead,
        &w.acme,
        Resource::Repository(&w.public),
        None,
    );
    assert_eq!(d, Decision::Deny(vec!["suspended-organization".into()]));
}

#[test]
fn anonymous_cannot_carry_grants_or_teams() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let teams = w.teams();
    let invalid = |d: Decision| assert!(matches!(d, Decision::Invalid(_)), "{d:?}");
    let grants = [grant(
        BuiltinRole::Read,
        Subject::Account(Uuid::new_v4()),
        Scope::Organization(w.acme.id),
    )];
    // Grants with an anonymous principal are a caller bug.
    invalid(engine.authorize(&Request {
        principal: Principal::Anonymous,
        teams: &[],
        grants: &grants,
        action: Action::RepositoryRead,
        organization: &w.acme,
        resource: Resource::Repository(&w.public),
        merge: None,
        auth_age: None,
        read_only: false,
    }));
    // So are teams.
    invalid(engine.authorize(&Request {
        principal: Principal::Anonymous,
        teams: &teams,
        grants: &[],
        action: Action::RepositoryRead,
        organization: &w.acme,
        resource: Resource::Repository(&w.public),
        merge: None,
        auth_age: None,
        read_only: false,
    }));
}

/// Even with a permit-everything role in the policy set, anonymous requests
/// can never link it: whatever else forbids, they deny cleanly.
#[test]
fn hostile_custom_role_reaches_no_anonymous_principal() {
    let mut engine = Engine::new().unwrap();
    engine
        .add_custom_role(
            "everything",
            "permit (principal in ?principal, action, resource in ?resource);",
        )
        .unwrap();
    let w = World::new();
    let site = site_on(&w.public, w.member.id, "refs/heads/main");
    for &action in Action::ALL {
        if is_public_read(action) {
            continue;
        }
        let (resource, merge) = site.resource(action);
        let d = w.anonymous(&engine, action, &w.acme, resource, merge);
        match d {
            Decision::Deny(ids) if ids.iter().any(|id| id == "anonymous-public-read-only") => {}
            other => panic!("{action:?}: expected anonymous-public-read-only, got {other:?}"),
        }
    }
}

/// Owned by the schema, not by the Rust table: a permit naming only the
/// schema's `sensitive` group allows exactly what `is_sensitive` says.
#[test]
fn rust_sensitivity_matches_the_schema_sensitive_group() {
    let mut engine = Engine::new().unwrap();
    engine
        .add_custom_role(
            "sensitive_only",
            r#"permit (principal in ?principal, action in [GitHearth::Action::"sensitive"], resource in ?resource);"#,
        )
        .unwrap();
    let w = World::new();
    // A site administrator: the site-admin-only forbid otherwise denies the
    // suspend and unsuspend actions, which also live in the sensitive group.
    let who = Account {
        site_admin: true,
        ..account(&[w.acme.id], &[])
    };
    // A private repository: no built-in permit matches, so only a permit
    // naming the sensitive group can allow anything.
    let site = site_on(&w.private, who.id, "refs/heads/pending");
    let grants = [Grant {
        role: Role::Custom("sensitive_only".into()),
        subject: Subject::Account(who.id),
        scope: Scope::Organization(w.acme.id),
    }];
    for &action in Action::ALL {
        let (resource, merge) = site.resource(action);
        let d = engine.authorize(&Request {
            principal: Principal::Account(&who),
            teams: &[],
            grants: &grants,
            action,
            organization: &w.acme,
            resource,
            merge,
            auth_age: Some(Duration::ZERO),
            read_only: false,
        });
        assert_eq!(d.is_allowed(), action.is_sensitive(), "{action:?}: {d:?}");
    }
}

/// Sudo mode: sensitive actions demand recently presented credentials from
/// the account; non-sensitive ones never do.
#[test]
fn sensitive_actions_require_fresh_authentication() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let fresh = |engine: &Engine, age: Option<Duration>| -> Decision {
        engine.authorize(&Request {
            principal: Principal::Account(&w.owner),
            teams: &[],
            grants: &[grant(
                BuiltinRole::Owner,
                Subject::Account(w.owner.id),
                Scope::Organization(w.acme.id),
            )],
            action: Action::OrganizationUpdate,
            organization: &w.acme,
            resource: Resource::Organization,
            merge: None,
            auth_age: age,
            read_only: false,
        })
    };
    assert!(fresh(&engine, Some(Duration::from_secs(60))).is_allowed());
    assert_eq!(
        fresh(&engine, Some(Duration::from_secs(16 * 60))),
        Decision::Deny(vec!["sensitive-requires-fresh-auth".into()])
    );
    // Unknown recency counts as stale.
    assert_eq!(
        fresh(&engine, None),
        Decision::Deny(vec!["sensitive-requires-fresh-auth".into()])
    );
    let longer = Engine::new()
        .unwrap()
        .with_fresh_auth(Duration::from_secs(60 * 60));
    assert!(fresh(&longer, Some(Duration::from_secs(30 * 60))).is_allowed());
    // Saturating beyond `Long` denies without panicking.
    assert!(!fresh(&engine, Some(Duration::MAX)).is_allowed());
    // Non-sensitive actions stay available without any recency.
    let d = engine.authorize(&Request {
        principal: Principal::Account(&w.owner),
        teams: &[],
        grants: &[grant(
            BuiltinRole::Owner,
            Subject::Account(w.owner.id),
            Scope::Organization(w.acme.id),
        )],
        action: Action::OrganizationRead,
        organization: &w.acme,
        resource: Resource::Organization,
        merge: None,
        auth_age: None,
        read_only: false,
    });
    assert!(d.is_allowed(), "{d:?}");
}

/// Role templates are cumulative: every higher role permits every action a
/// lower role permits (issue #133). Each role is granted at both organization
/// and repository scope so org-level and repository-level actions are both
/// covered. `read`/`triage` deliberately do not carry the base `member`
/// privilege to create repositories, so the chain checked here starts at
/// `read`; `member` is checked separately below `write`.
#[test]
fn roles_are_cumulative() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    let site = site_on(&w.private, w.member.id, "refs/heads/main");
    let chain = [
        BuiltinRole::Read,
        BuiltinRole::Triage,
        BuiltinRole::Write,
        BuiltinRole::Maintain,
        BuiltinRole::Admin,
        BuiltinRole::Owner,
    ];
    let permitted = |role: BuiltinRole| {
        let who = account(&[w.acme.id], &[]);
        let grants = [
            grant(
                role,
                Subject::Account(who.id),
                Scope::Organization(w.acme.id),
            ),
            grant(
                role,
                Subject::Account(who.id),
                Scope::Repository(w.private.id),
            ),
        ];
        Action::ALL
            .iter()
            .map(|&action| {
                let (resource, merge) = site.resource(action);
                engine
                    .authorize(&Request {
                        principal: Principal::Account(&who),
                        teams: &[],
                        grants: &grants,
                        action,
                        organization: &w.acme,
                        resource,
                        merge,
                        auth_age: Some(Duration::ZERO),
                        read_only: false,
                    })
                    .is_allowed()
            })
            .collect::<Vec<bool>>()
    };
    let table: Vec<Vec<bool>> = chain.iter().map(|&role| permitted(role)).collect();
    for (lower, lower_role) in chain.iter().enumerate() {
        for (higher, higher_role) in chain.iter().enumerate() {
            if higher <= lower {
                continue;
            }
            for (index, &action) in Action::ALL.iter().enumerate() {
                assert!(
                    !table[lower][index] || table[higher][index],
                    "{lower_role:?} permits {action:?} but {higher_role:?} does not"
                );
            }
        }
    }
    // The base `member` role sits beside the chain, not below `read`: it
    // carries `repository.create`, which `read`/`triage` do not, but every
    // role from `write` upward carries everything it permits.
    let member_table = permitted(BuiltinRole::Member);
    let create_index = Action::ALL
        .iter()
        .position(|&action| action == Action::RepositoryCreate)
        .unwrap();
    for (offset, &higher) in chain.iter().enumerate().skip(2) {
        for (index, &action) in Action::ALL.iter().enumerate() {
            assert!(
                !member_table[index] || table[offset][index],
                "member permits {action:?} but {higher:?} does not"
            );
        }
    }
    assert!(member_table[create_index], "member cannot create");
    assert!(!table[0][create_index], "read can create");
    assert!(!table[1][create_index], "triage can create");
}

/// `repository.create` lives in the `member` action group, which the base
/// `member` role and every grant from `write` upward must grant (issue #133):
/// an organization member with a `member`, `write`, `maintain` or `admin`
/// grant can create a repository, while a `read`/`triage` grant cannot (the
/// fork tests pin that) and an outsider cannot either.
#[test]
fn repository_create_availability_matches_the_role_ladder() {
    let engine = Engine::new().unwrap();
    let w = World::new();
    for (role, allowed) in [
        (BuiltinRole::Member, true),
        (BuiltinRole::Read, false),
        (BuiltinRole::Triage, false),
        (BuiltinRole::Write, true),
        (BuiltinRole::Maintain, true),
        (BuiltinRole::Admin, true),
        (BuiltinRole::Owner, true),
    ] {
        let who = account(&[w.acme.id], &[]);
        let grants = [grant(
            role,
            Subject::Account(who.id),
            Scope::Organization(w.acme.id),
        )];
        let d = engine.authorize(&Request {
            principal: Principal::Account(&who),
            teams: &[],
            grants: &grants,
            action: Action::RepositoryCreate,
            organization: &w.acme,
            resource: Resource::Organization,
            merge: None,
            auth_age: Some(Duration::ZERO),
            read_only: false,
        });
        assert_eq!(d.is_allowed(), allowed, "{role:?}: {d:?}");
    }
    // A visitor who is not a member of the organization cannot create there.
    let outsider = account(&[], &[]);
    let d = engine.authorize(&Request {
        principal: Principal::Account(&outsider),
        teams: &[],
        grants: &[],
        action: Action::RepositoryCreate,
        organization: &w.acme,
        resource: Resource::Organization,
        merge: None,
        auth_age: Some(Duration::ZERO),
        read_only: false,
    });
    assert!(!d.is_allowed(), "{d:?}");
}
