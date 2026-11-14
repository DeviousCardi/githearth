//! Policy-based authorization for GitHearth with Cedar
//! (docs/decisions/0002-authorization.md).
//!
//! One decision evaluates:
//! - the built-in policies (`policies/builtin.cedar`), whose forbids no role
//!   overrides;
//! - one linked role template per grant, so a grant to a team reaches its
//!   members and a grant on an organization reaches its repositories;
//! - entities built here from facts the caller loaded from authoritative
//!   storage. Callers never supply Cedar entities or context directly.
//!
//! The engine fails closed. Inconsistent facts, a request or entity the
//! schema rejects, and any policy evaluation error deny. Cedar on its own
//! skips a policy that fails to evaluate, which would turn a broken forbid
//! into an allow; here any error is a denial.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use cedar_policy::{
    Authorizer, Context, Decision as CedarDecision, Effect, Entities, Entity, EntityId,
    EntityTypeName, EntityUid, PolicyId, PolicySet, RestrictedExpression, Schema, SlotId, Template,
    TemplatePrincipalConstraint, TemplateResourceConstraint, ValidationMode, Validator,
};
use uuid::Uuid;

pub const SCHEMA: &str = include_str!("../policies/githearth.cedarschema");
pub const BUILTINS: &str = include_str!("../policies/builtin.cedar");

/// Largest custom role template accepted.
pub const MAX_ROLE_BYTES: usize = 16 * 1024;

/// How long after the last password entry an account may start sensitive
/// actions (sudo mode).
pub const DEFAULT_FRESH_AUTH: Duration = Duration::from_secs(15 * 60);

macro_rules! sensitivity {
    (sensitive) => {
        true
    };
    (routine) => {
        false
    };
}

macro_rules! actions {
    ($($variant:ident => $name:literal, $sensitivity:ident,)*) => {
        /// Every action a request can name. Group actions (`read`, `write`, …)
        /// exist only in the schema; policies use them, requests never do.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Action { $($variant,)* }
        impl Action {
            pub const ALL: &[Action] = &[$(Action::$variant,)*];
            pub fn name(self) -> &'static str {
                match self { $(Action::$variant => $name,)* }
            }
            /// Whether the action changes access, administers or destroys,
            /// and so requires fresh authentication.
            pub fn is_sensitive(self) -> bool {
                match self { $(Action::$variant => sensitivity!($sensitivity),)* }
            }
        }
    };
}
actions! {
    OrganizationRead => "organization.read", routine,
    OrganizationUpdate => "organization.update", sensitive,
    OrganizationManageMembers => "organization.manage_members", sensitive,
    OrganizationDelete => "organization.delete", sensitive,
    OrganizationSuspend => "organization.suspend", sensitive,
    OrganizationUnsuspend => "organization.unsuspend", sensitive,
    AccountRead => "account.read", sensitive,
    AccountCreate => "account.create", sensitive,
    AccountDisable => "account.disable", sensitive,
    AccountEnable => "account.enable", sensitive,
    AccountResetPassword => "account.reset_password", sensitive,
    TeamCreate => "team.create", sensitive,
    RepositoryCreate => "repository.create", routine,
    RepositoryRead => "repository.read", routine,
    RepositoryClone => "repository.clone", routine,
    RepositoryUpdate => "repository.update", routine,
    RepositoryManageAccess => "repository.manage_access", sensitive,
    RepositoryArchive => "repository.archive", sensitive,
    RepositoryUnarchive => "repository.unarchive", sensitive,
    RepositoryDelete => "repository.delete", sensitive,
    RefPush => "ref.push", routine,
    RefForcePush => "ref.force_push", sensitive,
    RefDelete => "ref.delete", sensitive,
    StatusCreate => "status.create", routine,
    IssueCreate => "issue.create", routine,
    IssueRead => "issue.read", routine,
    IssueComment => "issue.comment", routine,
    IssueLabel => "issue.label", routine,
    IssueAssign => "issue.assign", routine,
    IssueClose => "issue.close", routine,
    PullRequestCreate => "pull_request.create", routine,
    PullRequestRead => "pull_request.read", routine,
    PullRequestComment => "pull_request.comment", routine,
    PullRequestClose => "pull_request.close", routine,
    PullRequestApprove => "pull_request.approve", routine,
    PullRequestMerge => "pull_request.merge", routine,
}

impl Action {
    /// The actions a read-only scoped token may still perform: everything in
    /// the schema's `read` / `public_read` groups, and nothing that mutates.
    pub fn read_only_permitted(self) -> bool {
        matches!(
            self,
            Action::OrganizationRead
                | Action::RepositoryRead
                | Action::RepositoryClone
                | Action::IssueRead
                | Action::PullRequestRead
        )
    }

    fn resource_kind(self) -> Kind {
        use Action::*;
        match self {
            OrganizationRead
            | OrganizationUpdate
            | OrganizationManageMembers
            | OrganizationDelete
            | OrganizationSuspend
            | OrganizationUnsuspend
            | TeamCreate
            | RepositoryCreate => Kind::Organization,
            AccountRead | AccountCreate | AccountDisable | AccountEnable | AccountResetPassword => {
                Kind::Instance
            }
            RepositoryRead
            | RepositoryClone
            | RepositoryUpdate
            | RepositoryManageAccess
            | RepositoryArchive
            | RepositoryUnarchive
            | RepositoryDelete
            | IssueCreate
            | PullRequestCreate => Kind::Repository,
            RefPush | RefForcePush | RefDelete => Kind::Ref,
            IssueRead | IssueComment | IssueLabel | IssueAssign | IssueClose => Kind::Issue,
            PullRequestRead | PullRequestComment | PullRequestClose | PullRequestApprove
            | PullRequestMerge => Kind::PullRequest,
            StatusCreate => Kind::Repository,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The instance itself: site-level administration that no organization
    /// scope covers (account provisioning and lifecycle).
    Instance,
    Organization,
    Repository,
    Ref,
    Issue,
    PullRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuiltinRole {
    Member,
    Read,
    Triage,
    Write,
    Maintain,
    Admin,
    Owner,
}
impl BuiltinRole {
    pub const ALL: [BuiltinRole; 7] = [
        Self::Member,
        Self::Read,
        Self::Triage,
        Self::Write,
        Self::Maintain,
        Self::Admin,
        Self::Owner,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Read => "read",
            Self::Triage => "triage",
            Self::Write => "write",
            Self::Maintain => "maintain",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }
    fn template(self) -> &'static str {
        match self {
            Self::Member => include_str!("../policies/roles/member.cedar"),
            Self::Read => include_str!("../policies/roles/read.cedar"),
            Self::Triage => include_str!("../policies/roles/triage.cedar"),
            Self::Write => include_str!("../policies/roles/write.cedar"),
            Self::Maintain => include_str!("../policies/roles/maintain.cedar"),
            Self::Admin => include_str!("../policies/roles/admin.cedar"),
            Self::Owner => include_str!("../policies/roles/owner.cedar"),
        }
    }
    /// Membership roles describe the organization itself and are never
    /// granted on a single repository.
    pub fn organization_only(self) -> bool {
        matches!(self, Self::Member | Self::Owner)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Builtin(BuiltinRole),
    Custom(String),
}
impl Role {
    fn template_id(&self) -> PolicyId {
        match self {
            Role::Builtin(role) => PolicyId::new(format!("role.{}", role.name())),
            Role::Custom(name) => PolicyId::new(format!("custom.{name}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subject {
    Account(Uuid),
    Team(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    Organization(Uuid),
    Repository(Uuid),
}

/// A role granted to an account or team over an organization or repository.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Grant {
    pub role: Role,
    pub subject: Subject,
    pub scope: Scope,
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: Uuid,
    pub suspended: bool,
    pub site_admin: bool,
    /// Organizations the account is a member of.
    pub organizations: Vec<Uuid>,
    /// Teams the account belongs to directly.
    pub teams: Vec<Uuid>,
}

#[derive(Clone, Debug)]
pub struct Team {
    pub id: Uuid,
    pub organization: Uuid,
    pub parent: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct Organization {
    pub id: Uuid,
    pub suspended: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Internal,
    Private,
}
impl Visibility {
    fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Private => "private",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Repository {
    pub id: Uuid,
    pub organization: Uuid,
    pub visibility: Visibility,
    pub archived: bool,
}

/// Branch protection that applies to a ref, after rule matching.
#[derive(Clone, Debug, Default)]
pub struct Protection {
    pub allow_force_push: bool,
    pub allow_deletion: bool,
    pub require_pull_request: bool,
    pub required_approvals: u32,
    pub require_checks: bool,
    /// Approvals count only at the current head; a moved head dismisses older
    /// approvals. Evaluated by the service when it counts approvals, not by the
    /// policy engine (which only compares the resulting count).
    pub dismiss_stale_approvals: bool,
    /// Pushes must carry verified signatures for every new commit. Evaluated by
    /// the service against Git, not by the policy engine.
    pub require_signed_commits: bool,
    pub require_code_owner_review: bool,
}

#[derive(Clone, Debug)]
pub struct Ref {
    pub repository: Uuid,
    /// Full ref name, such as `refs/heads/main`.
    pub name: String,
    pub protection: Option<Protection>,
    /// Whether the ref already exists: false only while it is being created
    /// (old oid all zeros), which the require-pull-request rule lets through.
    pub exists: bool,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub id: Uuid,
    pub repository: Uuid,
    pub author: Uuid,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub id: Uuid,
    pub repository: Uuid,
    pub author: Uuid,
    pub target: Ref,
}

#[derive(Clone, Copy, Debug)]
pub enum Resource<'a> {
    /// The instance: used for site administration that is not scoped to an
    /// organization, such as creating an account.
    Instance,
    Organization,
    Repository(&'a Repository),
    Ref(&'a Repository, &'a Ref),
    Issue(&'a Repository, &'a Issue),
    PullRequest(&'a Repository, &'a PullRequest),
}
impl Resource<'_> {
    fn kind(&self) -> Kind {
        match self {
            Resource::Instance => Kind::Instance,
            Resource::Organization => Kind::Organization,
            Resource::Repository(_) => Kind::Repository,
            Resource::Ref(..) => Kind::Ref,
            Resource::Issue(..) => Kind::Issue,
            Resource::PullRequest(..) => Kind::PullRequest,
        }
    }
}

/// Merge facts computed by the service for the exact head and target
/// revisions being merged.
#[derive(Clone, Copy, Debug)]
pub struct Merge {
    pub approvals: u32,
    pub checks_passed: bool,
    pub code_owner_review_ok: bool,
}

/// The requesting principal: a signed-in account, or an unauthenticated
/// visitor, who carries no grants, no teams and no account attributes.
#[derive(Clone, Copy, Debug)]
pub enum Principal<'a> {
    Account(&'a Account),
    Anonymous,
}

#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub principal: Principal<'a>,
    /// Every team in `principal.teams` and all of their ancestors.
    pub teams: &'a [Team],
    /// Grants whose subject is the principal or one of those teams.
    pub grants: &'a [Grant],
    pub action: Action,
    /// The organization that owns the resource.
    pub organization: &'a Organization,
    pub resource: Resource<'a>,
    /// Required for `pull_request.merge`, refused otherwise.
    pub merge: Option<Merge>,
    /// Time since the account last presented its password; `None` for
    /// anonymous or unknown recency, which counts as stale.
    pub auth_age: Option<Duration>,
    /// True when the credential is a scoped personal access token without
    /// `repo:write`, restricting it to read-only actions regardless of the
    /// account's grants.
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed by the listed permits.
    Allow(Vec<String>),
    /// Denied by the listed forbids, or by no permit when the list is empty.
    Deny(Vec<String>),
    /// Denied before or during evaluation: inconsistent facts, schema
    /// mismatch or an evaluation error.
    Invalid(&'static str),
}
impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
fn error(context: &str, detail: impl fmt::Display) -> Error {
    Error(format!("{context}: {detail}"))
}

pub struct Engine {
    schema: Schema,
    validator: Validator,
    /// Built-ins plus every role template, ready for linking.
    base: PolicySet,
    custom: HashSet<String>,
    /// Sudo-mode window for sensitive actions.
    fresh_auth: Duration,
    authorizer: Authorizer,
}

impl Engine {
    /// Load and validate the schema, built-in policies and role templates.
    pub fn new() -> Result<Self, Error> {
        let (schema, _) = Schema::from_cedarschema_str(SCHEMA).map_err(|e| error("schema", e))?;
        let validator = Validator::new(schema.clone());
        let mut base = PolicySet::new();
        let parsed = PolicySet::from_str(BUILTINS).map_err(|e| error("built-ins", e))?;
        if parsed.templates().next().is_some() {
            return Err(Error("built-ins: templates are not allowed".into()));
        }
        for policy in parsed.policies() {
            let id = policy
                .annotation("id")
                .ok_or_else(|| Error("built-ins: every policy needs an @id".into()))?;
            base.add(policy.new_id(PolicyId::new(id)))
                .map_err(|e| error("built-ins", e))?;
        }
        for role in BuiltinRole::ALL {
            let role = Role::Builtin(role);
            let template = role_template(&role, template_text(&role))?;
            base.add_template(template).map_err(|e| error("role", e))?;
        }
        let engine = Self {
            schema,
            validator,
            base,
            custom: HashSet::new(),
            fresh_auth: DEFAULT_FRESH_AUTH,
            authorizer: Authorizer::new(),
        };
        engine.validate(&engine.base)?;
        Ok(engine)
    }

    /// Move the sudo-mode window for sensitive actions.
    pub fn with_fresh_auth(mut self, window: Duration) -> Self {
        self.fresh_auth = window;
        self
    }

    /// Add an organization-defined role. The template must be a single
    /// `permit (principal in ?principal, action …, resource in ?resource)`
    /// that validates against the schema. It can never override a built-in
    /// forbid.
    pub fn add_custom_role(&mut self, name: &str, text: &str) -> Result<(), Error> {
        let valid_name = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !valid_name || BuiltinRole::ALL.iter().any(|r| r.name() == name) {
            return Err(Error("custom role: invalid or reserved name".into()));
        }
        if text.len() > MAX_ROLE_BYTES {
            return Err(Error("custom role: template too large".into()));
        }
        if self.custom.contains(name) {
            return Err(Error("custom role: already defined".into()));
        }
        let role = Role::Custom(name.to_owned());
        let template = role_template(&role, text)?;
        let mut candidate = self.base.clone();
        candidate
            .add_template(template)
            .map_err(|e| error("custom role", e))?;
        self.validate(&candidate)?;
        self.base = candidate;
        self.custom.insert(name.to_owned());
        Ok(())
    }

    fn validate(&self, set: &PolicySet) -> Result<(), Error> {
        let result = self.validator.validate(set, ValidationMode::Strict);
        match result.validation_errors().next() {
            Some(e) => Err(error("validation", e)),
            None => Ok(()),
        }
    }

    pub fn authorize(&self, request: &Request<'_>) -> Decision {
        match self.evaluate(request) {
            Ok(decision) => decision,
            Err(reason) => Decision::Invalid(reason),
        }
    }

    fn evaluate(&self, request: &Request<'_>) -> Result<Decision, &'static str> {
        check_facts(request)?;
        let mut policies = self.base.clone();
        for (i, grant) in request.grants.iter().enumerate() {
            if !self.usable(grant) {
                // An unknown role or a membership role on a repository grants nothing.
                continue;
            }
            let subject = match grant.subject {
                Subject::Account(id) => uid("Account", id.to_string()),
                Subject::Team(id) => uid("Team", id.to_string()),
            };
            let scope = match grant.scope {
                Scope::Organization(id) => uid("Organization", id.to_string()),
                Scope::Repository(id) => uid("Repository", id.to_string()),
            };
            let id = PolicyId::new(format!("grant.{i}.{}", grant.role.template_id()));
            policies
                .link(
                    grant.role.template_id(),
                    id,
                    HashMap::from([(SlotId::principal(), subject), (SlotId::resource(), scope)]),
                )
                .map_err(|_| "role link failed")?;
        }
        let (resource, entities) = build_entities(request);
        let entities = Entities::from_entities(entities, Some(&self.schema))
            .map_err(|_| "entities do not match the schema")?;
        let mut context_pairs = vec![
            (
                "auth_age_seconds".to_owned(),
                RestrictedExpression::new_long(request.auth_age.map_or(i64::MAX, duration_seconds)),
            ),
            (
                "fresh_auth_seconds".to_owned(),
                RestrictedExpression::new_long(duration_seconds(self.fresh_auth)),
            ),
        ];
        if let Some(merge) = request.merge {
            context_pairs.push((
                "approvals".to_owned(),
                RestrictedExpression::new_long(i64::from(merge.approvals)),
            ));
            context_pairs.push((
                "checks_passed".to_owned(),
                RestrictedExpression::new_bool(merge.checks_passed),
            ));
            context_pairs.push((
                "code_owner_review_ok".to_owned(),
                RestrictedExpression::new_bool(merge.code_owner_review_ok),
            ));
        }
        let context = Context::from_pairs(context_pairs).map_err(|_| "invalid context")?;
        let action = uid("Action", request.action.name().to_owned());
        let principal = match request.principal {
            Principal::Account(account) => uid("Account", account.id.to_string()),
            Principal::Anonymous => uid("Anonymous", String::new()),
        };
        let cedar_request =
            cedar_policy::Request::new(principal, action, resource, context, Some(&self.schema))
                .map_err(|_| "request does not match the schema")?;
        let response = self
            .authorizer
            .is_authorized(&cedar_request, &policies, &entities);
        if response.diagnostics().errors().next().is_some() {
            return Err("policy evaluation error");
        }
        let reasons = response
            .diagnostics()
            .reason()
            .map(ToString::to_string)
            .collect();
        Ok(match response.decision() {
            CedarDecision::Allow => Decision::Allow(reasons),
            CedarDecision::Deny => Decision::Deny(reasons),
        })
    }

    fn usable(&self, grant: &Grant) -> bool {
        match &grant.role {
            Role::Builtin(role) => {
                !(role.organization_only() && matches!(grant.scope, Scope::Repository(_)))
            }
            Role::Custom(name) => self.custom.contains(name),
        }
    }
}

fn template_text(role: &Role) -> &'static str {
    match role {
        Role::Builtin(role) => role.template(),
        Role::Custom(_) => unreachable!("custom roles bring their own text"),
    }
}

fn role_template(role: &Role, text: &str) -> Result<Template, Error> {
    let template = Template::parse(Some(role.template_id()), text).map_err(|e| error("role", e))?;
    let shape = template.effect() == Effect::Permit
        && matches!(
            template.principal_constraint(),
            TemplatePrincipalConstraint::In(None)
        )
        && matches!(
            template.resource_constraint(),
            TemplateResourceConstraint::In(None)
        );
    if !shape {
        return Err(Error(
            "role: must be `permit (principal in ?principal, action …, resource in ?resource)`"
                .into(),
        ));
    }
    Ok(template)
}

/// Facts must describe one consistent object graph. Anything else is a
/// caller bug or stale data, and denies.
fn check_facts(request: &Request<'_>) -> Result<(), &'static str> {
    if request.action.resource_kind() != request.resource.kind() {
        return Err("action does not apply to this resource");
    }
    if request.read_only && !request.action.read_only_permitted() {
        return Err("read-only credential forbids this action");
    }
    if request.merge.is_some() != (request.action == Action::PullRequestMerge) {
        return Err("merge facts are required for merges only");
    }
    let org = request.organization.id;
    let repository = match request.resource {
        Resource::Instance | Resource::Organization => None,
        Resource::Repository(r) => Some(r),
        Resource::Ref(r, reference) => (reference.repository == r.id)
            .then_some(r)
            .ok_or("ref is not in the repository")
            .map(Some)?,
        Resource::Issue(r, issue) => (issue.repository == r.id)
            .then_some(r)
            .ok_or("issue is not in the repository")
            .map(Some)?,
        Resource::PullRequest(r, pr) => (pr.repository == r.id && pr.target.repository == r.id)
            .then_some(r)
            .ok_or("pull request is not in the repository")
            .map(Some)?,
    };
    if repository.is_some_and(|r| r.organization != org) {
        return Err("repository is not in the organization");
    }
    let teams: HashMap<Uuid, &Team> = request.teams.iter().map(|t| (t.id, t)).collect();
    if teams.len() != request.teams.len() {
        return Err("duplicate team");
    }
    let (principal_teams, organizations): (&[Uuid], &[Uuid]) = match request.principal {
        Principal::Account(account) => (&account.teams, &account.organizations),
        Principal::Anonymous if request.grants.is_empty() && request.teams.is_empty() => (&[], &[]),
        Principal::Anonymous => return Err("anonymous principal carries no grants or teams"),
    };
    for id in principal_teams {
        let team = teams.get(id).ok_or("principal team missing")?;
        if !organizations.contains(&team.organization) {
            return Err("team outside the principal's organizations");
        }
    }
    for team in request.teams {
        if let Some(parent) = team.parent {
            let parent = teams.get(&parent).ok_or("parent team missing")?;
            if parent.organization != team.organization {
                return Err("parent team in another organization");
            }
        }
    }
    Ok(())
}

fn uid(kind: &str, id: String) -> EntityUid {
    let name = EntityTypeName::from_str(&format!("GitHearth::{kind}"))
        .expect("schema entity type names are valid");
    EntityUid::from_type_name_and_id(name, EntityId::new(id))
}

/// Seconds of a duration, saturating beyond what Cedar's `Long` holds.
fn duration_seconds(duration: Duration) -> i64 {
    duration.as_secs().try_into().unwrap_or(i64::MAX)
}

fn entity(
    uid: EntityUid,
    attrs: impl IntoIterator<Item = (&'static str, RestrictedExpression)>,
    parents: impl IntoIterator<Item = EntityUid>,
) -> Entity {
    let attrs = attrs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    Entity::new(uid, attrs, parents.into_iter().collect())
        .expect("attribute expressions are literals")
}

fn ref_uid(reference: &Ref) -> EntityUid {
    // A repository holds one ref per name; the UUID prefix scopes the name.
    uid(
        "Ref",
        format!("{}:{}", reference.repository, reference.name),
    )
}

/// Returns the resource UID and every entity the decision may read.
fn build_entities(request: &Request<'_>) -> (EntityUid, Vec<Entity>) {
    let bool_ = RestrictedExpression::new_bool;
    let ref_ = RestrictedExpression::new_entity_uid;
    let org = request.organization;
    let org_uid = uid("Organization", org.id.to_string());
    let mut entities = vec![entity(
        org_uid.clone(),
        [("suspended", bool_(org.suspended))],
        [],
    )];
    match request.principal {
        Principal::Account(account) => entities.push(entity(
            uid("Account", account.id.to_string()),
            [
                ("suspended", bool_(account.suspended)),
                ("site_admin", bool_(account.site_admin)),
            ],
            account
                .organizations
                .iter()
                .map(|id| uid("Organization", id.to_string()))
                .chain(account.teams.iter().map(|id| uid("Team", id.to_string()))),
        )),
        Principal::Anonymous => {
            entities.push(entity(uid("Anonymous", String::new()), [], []));
        }
    }
    for team in request.teams {
        let team_org = uid("Organization", team.organization.to_string());
        entities.push(entity(
            uid("Team", team.id.to_string()),
            [("organization", ref_(team_org.clone()))],
            std::iter::once(team_org).chain(team.parent.map(|p| uid("Team", p.to_string()))),
        ));
    }
    let repository = |r: &Repository| {
        entity(
            uid("Repository", r.id.to_string()),
            [
                ("organization", ref_(org_uid.clone())),
                (
                    "visibility",
                    RestrictedExpression::new_string(r.visibility.name().to_owned()),
                ),
                ("archived", bool_(r.archived)),
            ],
            [org_uid.clone()],
        )
    };
    let reference = |r: &Ref| {
        let p = r.protection.clone();
        let protected = p.is_some();
        let p = p.unwrap_or(Protection {
            allow_force_push: true,
            allow_deletion: true,
            ..Protection::default()
        });
        let repo_uid = uid("Repository", r.repository.to_string());
        entity(
            ref_uid(r),
            [
                ("organization", ref_(org_uid.clone())),
                ("repository", ref_(repo_uid.clone())),
                ("protected", bool_(protected)),
                ("allow_force_push", bool_(p.allow_force_push)),
                ("allow_deletion", bool_(p.allow_deletion)),
                ("require_pull_request", bool_(p.require_pull_request)),
                (
                    "required_approvals",
                    RestrictedExpression::new_long(i64::from(p.required_approvals)),
                ),
                ("require_checks", bool_(p.require_checks)),
                (
                    "require_code_owner_review",
                    bool_(p.require_code_owner_review),
                ),
                ("exists", bool_(r.exists)),
            ],
            [repo_uid],
        )
    };
    let owned = |r: &Repository, author: Uuid| {
        [
            ("organization", ref_(org_uid.clone())),
            ("repository", ref_(uid("Repository", r.id.to_string()))),
            ("author", ref_(uid("Account", author.to_string()))),
        ]
    };
    let resource = match request.resource {
        Resource::Instance => {
            let id = uid("Instance", String::new());
            entities.push(entity(id.clone(), [], []));
            id
        }
        Resource::Organization => org_uid.clone(),
        Resource::Repository(r) => {
            entities.push(repository(r));
            uid("Repository", r.id.to_string())
        }
        Resource::Ref(r, target) => {
            entities.push(repository(r));
            entities.push(reference(target));
            ref_uid(target)
        }
        Resource::Issue(r, issue) => {
            entities.push(repository(r));
            let id = uid("Issue", issue.id.to_string());
            let repo_uid = uid("Repository", r.id.to_string());
            entities.push(entity(id.clone(), owned(r, issue.author), [repo_uid]));
            id
        }
        Resource::PullRequest(r, pr) => {
            entities.push(repository(r));
            entities.push(reference(&pr.target));
            let id = uid("PullRequest", pr.id.to_string());
            let repo_uid = uid("Repository", r.id.to_string());
            let attrs = owned(r, pr.author)
                .into_iter()
                .chain([("target", ref_(ref_uid(&pr.target)))]);
            entities.push(entity(id.clone(), attrs, [repo_uid]));
            id
        }
    };
    (resource, entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_loads_and_validates() {
        Engine::new().unwrap();
    }

    #[test]
    fn rust_actions_match_the_schema() {
        let schema = Engine::new().unwrap().schema;
        let groups = [
            "public_read",
            "member",
            "read",
            "triage",
            "write",
            "maintain",
            "admin",
            "owner",
            "site_admin",
            "sensitive",
        ];
        let in_schema: HashSet<String> = schema
            .actions()
            .map(|a| a.id().unescaped().to_owned())
            .filter(|name| !groups.contains(&name.as_str()))
            .collect();
        let in_rust: HashSet<String> = Action::ALL.iter().map(|a| a.name().to_owned()).collect();
        assert_eq!(in_schema, in_rust);
    }

    #[test]
    fn custom_roles_must_be_principal_and_resource_permit_templates() {
        let mut engine = Engine::new().unwrap();
        for text in [
            "forbid (principal in ?principal, action, resource in ?resource);",
            "permit (principal == ?principal, action, resource in ?resource);",
            "permit (principal in ?principal, action, resource);",
            "permit (principal, action, resource);",
            "permit (principal in ?principal, action, resource in ?resource) when { resource.nonexistent };",
        ] {
            assert!(engine.add_custom_role("bad", text).is_err(), "{text}");
        }
        assert!(
            engine
                .add_custom_role(
                    "read",
                    "permit (principal in ?principal, action, resource in ?resource);"
                )
                .is_err()
        );
        engine
            .add_custom_role(
                "labeler",
                r#"permit (principal in ?principal, action in [GitHearth::Action::"issue.label"], resource in ?resource);"#,
            )
            .unwrap();
        assert!(
            engine
                .add_custom_role(
                    "labeler",
                    "permit (principal in ?principal, action, resource in ?resource);"
                )
                .is_err()
        );
    }
}
