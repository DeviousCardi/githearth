//! CODEOWNERS parsing, matching and the automatic review-request job.
//!
//! The CODEOWNERS file (`.githearth/CODEOWNERS`, then `CODEOWNERS`, then
//! `docs/CODEOWNERS`) lives on the pull request's target branch. It maps
//! gitignore-style patterns to owners (a username, or an `org/team`). For a
//! given path the last matching rule wins. When a pull request is opened or its
//! head changes, a durable job requests a review from every owner of the
//! changed files; branch protection can then require that every changed file
//! with owners has an approving review from one of them.

use githearth_git::{StorageError, browse};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

use crate::{ApiError, AppState, jobs, pulls, repositories};

/// Where to look for a CODEOWNERS file, first match wins.
const CODEOWNERS_PATHS: [&str; 3] = [".githearth/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

// ---------------------------------------------------------------------------
// Pure parsing and matching
// ---------------------------------------------------------------------------

/// A code owner as written in the file: `@username` or `@org/team`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum OwnerRef {
    Account(String),
    Team { org: String, team: String },
}

impl OwnerRef {
    /// The display form (`@username` / `@org/team`).
    pub(crate) fn display(&self) -> String {
        match self {
            OwnerRef::Account(username) => format!("@{username}"),
            OwnerRef::Team { org, team } => format!("@{org}/{team}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    pub(crate) pattern: String,
    pub(crate) owners: Vec<OwnerRef>,
}

/// Parses CODEOWNERS content into rules in file order. Blank lines and comment
/// lines (a `#` after optional whitespace) are skipped; a `\` escapes the next
/// character so patterns and owners may contain spaces or `#`. Malformed owner
/// tokens are skipped but returned as human-readable errors.
pub(crate) fn parse(content: &str) -> (Vec<Rule>, Vec<String>) {
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let tokens = tokenize(line);
        if tokens.is_empty() {
            continue;
        }
        let pattern = tokens[0].clone();
        let mut owners = Vec::new();
        for token in &tokens[1..] {
            match parse_owner(token) {
                Ok(owner) => owners.push(owner),
                Err(message) => errors.push(message),
            }
        }
        // A pattern with no owners names nobody; keep it out of the way.
        rules.push(Rule { pattern, owners });
    }
    (rules, errors)
}

/// Splits a line on unescaped whitespace, honouring `\` escapes.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_owner(token: &str) -> Result<OwnerRef, String> {
    let Some(stripped) = token.strip_prefix('@') else {
        return Err(format!("malformed owner token `{token}`"));
    };
    if stripped.is_empty() {
        return Err(format!("malformed owner token `{token}`"));
    }
    match stripped.split_once('/') {
        Some((org, team)) if !org.is_empty() && !team.is_empty() && !team.contains('/') => {
            Ok(OwnerRef::Team {
                org: org.to_owned(),
                team: team.to_owned(),
            })
        }
        Some(_) => Err(format!("malformed owner token `{token}`")),
        None => Ok(OwnerRef::Account(stripped.to_owned())),
    }
}

/// The owners of `path` under the last matching rule, or empty when no rule
/// matches.
pub(crate) fn owners_for(rules: &[Rule], path: &str) -> Vec<OwnerRef> {
    let mut owners: Vec<OwnerRef> = Vec::new();
    for rule in rules {
        if pattern_matches(&rule.pattern, path) {
            owners = rule.owners.clone();
        }
    }
    owners
}

/// Gitignore-style pattern match. `*` and `?` do not cross `/`; `**` does.
/// A leading `/` anchors to the root; a pattern without any `/` matches a
/// basename at any depth; a trailing `/` matches a directory and everything
/// under it.
pub(crate) fn pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern.ends_with('/') {
        // A trailing slash matches the directory's contents only, never a file
        // that happens to share the directory's name. `dir/**` alone would also
        // match the path `dir` itself (`**` may consume no segments), so the
        // bare directory is excluded.
        let dir = pattern.trim_end_matches('/');
        return path != dir && match_core(&format!("{dir}/**"), path);
    }
    match_core(pattern, path)
}

fn match_core(pattern: &str, path: &str) -> bool {
    let anchored = pattern.starts_with('/');
    let pattern = pattern.trim_start_matches('/');
    if anchored || pattern.contains('/') {
        let ps: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let ts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match_segments(&ps, &ts)
    } else {
        path.split('/')
            .filter(|s| !s.is_empty())
            .any(|seg| match_segments(&[pattern], &[seg]))
    }
}

/// Matches pattern segments against path segments; `**` consumes zero or more
/// whole segments.
fn match_segments(ps: &[&str], ts: &[&str]) -> bool {
    if ps.is_empty() {
        return ts.is_empty();
    }
    if ps[0] == "**" {
        for i in 0..=ts.len() {
            if match_segments(&ps[1..], &ts[i..]) {
                return true;
            }
        }
        return false;
    }
    if ts.is_empty() {
        return false;
    }
    if match_glob_segment(ps[0], ts[0]) {
        match_segments(&ps[1..], &ts[1..])
    } else {
        false
    }
}

/// Matches a single path segment: `*`, `?` and `[...]` character classes.
fn match_glob_segment(pattern: &str, text: &str) -> bool {
    wildmatch(pattern.as_bytes(), text.as_bytes())
}

fn wildmatch(p: &[u8], t: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut ti = 0usize;
    // The position just past a `*` and the text position it started at.
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        let mut advanced = false;
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star = Some((pi + 1, ti));
                    pi += 1;
                    advanced = true;
                }
                b'?' => {
                    pi += 1;
                    ti += 1;
                    advanced = true;
                }
                b'[' => {
                    if let Some((matched, next)) = match_char_class(p, pi, t[ti]) {
                        pi = next;
                        if matched {
                            ti += 1;
                            advanced = true;
                        }
                    } else if p[pi] == t[ti] {
                        pi += 1;
                        ti += 1;
                        advanced = true;
                    }
                }
                c if c == t[ti] => {
                    pi += 1;
                    ti += 1;
                    advanced = true;
                }
                _ => {}
            }
        }
        if advanced {
            continue;
        }
        match star {
            Some((sp, st)) => {
                pi = sp;
                ti = st + 1;
                star = Some((sp, st + 1));
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Parses a character class at `p[start] == b'['`. Returns
/// `Some((matched, next_index))` for a well-formed class, or `None` so the
/// caller treats the bracket as a literal.
fn match_char_class(p: &[u8], start: usize, c: u8) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let mut negate = false;
    if i < p.len() && (p[i] == b'!' || p[i] == b'^') {
        negate = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Server-side resolution
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct RequiredOwner {
    pub(crate) name: String,
    pub(crate) approved: bool,
}

#[derive(Clone)]
pub(crate) struct CodeOwnerReview {
    pub(crate) required: Vec<RequiredOwner>,
    pub(crate) satisfied: bool,
    pub(crate) errors: Vec<String>,
}

struct ResolvedOwner {
    name: String,
    account_ids: Vec<Uuid>,
}

/// Where the CODEOWNERS file lives on the target branch, and whether it could
/// be read. The first path that resolves to a file is authoritative: if that
/// file is binary, oversized or not UTF-8 the gate must fail closed rather than
/// fall back to a later path.
enum CodeOwnersBlob {
    Missing,
    Readable(String),
    Unreadable(String),
}

fn read_codeowners_blob(
    repo: &Path,
    rev: &str,
    timeout: Duration,
) -> Result<CodeOwnersBlob, StorageError> {
    for path in CODEOWNERS_PATHS {
        let Some(blob) = browse::blob(repo, rev, path, browse::MAX_BLOB_BYTES, timeout)? else {
            continue;
        };
        if blob.binary {
            return Ok(CodeOwnersBlob::Unreadable(format!(
                "{path} is not a text file"
            )));
        }
        if blob.truncated {
            return Ok(CodeOwnersBlob::Unreadable(format!(
                "{path} exceeds the size limit"
            )));
        }
        match String::from_utf8(blob.content) {
            Ok(text) => return Ok(CodeOwnersBlob::Readable(text)),
            Err(_) => {
                return Ok(CodeOwnersBlob::Unreadable(format!(
                    "{path} is not valid UTF-8"
                )));
            }
        }
    }
    Ok(CodeOwnersBlob::Missing)
}

/// The CODEOWNERS file and changed files for a pull request, with enough
/// context for the review gate to fail closed.
pub(crate) struct CodeOwnersLoad {
    pub(crate) rules: Vec<Rule>,
    pub(crate) paths: Option<Vec<String>>,
    pub(crate) errors: Vec<String>,
    pub(crate) diff_truncated: bool,
    pub(crate) unreadable: bool,
}

/// Reads the CODEOWNERS file from `base` and the changed files between `base`
/// and `head`, in one blocking Git pass.
pub(crate) async fn load(
    state: &AppState,
    repo_id: Uuid,
    base: &str,
    head: &str,
) -> Result<CodeOwnersLoad, ApiError> {
    let base = base.to_owned();
    let head = head.to_owned();
    let (content, paths) = crate::browse::git_read(state, repo_id, move |fs_path, timeout| {
        let content = read_codeowners_blob(fs_path, &base, timeout)?;
        let paths =
            browse::changed_files(fs_path, &base, &head, timeout)?.map(|(files, truncated)| {
                (
                    files.into_iter().map(|f| f.new_path).collect::<Vec<_>>(),
                    truncated,
                )
            });
        Ok::<_, StorageError>((content, paths))
    })
    .await?;

    let (rules, errors, unreadable) = match content {
        CodeOwnersBlob::Missing => (Vec::new(), Vec::new(), false),
        CodeOwnersBlob::Readable(text) => {
            let (rules, errors) = parse(&text);
            (rules, errors, false)
        }
        CodeOwnersBlob::Unreadable(reason) => (Vec::new(), vec![reason], true),
    };
    let (paths, diff_truncated) = match paths {
        Some((paths, truncated)) => (Some(paths), truncated),
        None => (None, false),
    };

    Ok(CodeOwnersLoad {
        rules,
        paths,
        errors,
        diff_truncated,
        unreadable,
    })
}

/// The reviewer ids whose latest non-comment review at `head` is an approval.
async fn approving_reviewers(
    conn: &mut PgConnection,
    pull_id: Uuid,
    head: &str,
) -> Result<HashSet<Uuid>, sqlx::Error> {
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT reviewer_id FROM (\
           SELECT DISTINCT ON (reviewer_id) verdict, reviewer_id FROM pull_request_reviews \
           WHERE pull_request_id = $1 AND commit_id = $2 AND verdict <> 'commented' \
           ORDER BY reviewer_id, seq DESC\
         ) latest WHERE verdict = 'approved'",
    )
    .bind(pull_id)
    .bind(head)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Resolves owners to their account ids: a username to its account, an
/// `org/team` (whose org matches the repository's) to its members. Owners that
/// cannot be resolved carry no accounts and therefore never satisfy the gate.
async fn resolve_owners(
    conn: &mut PgConnection,
    org_id: Uuid,
    org_slug: &str,
    owners: &[OwnerRef],
) -> Result<Vec<ResolvedOwner>, sqlx::Error> {
    let usernames: Vec<&str> = owners
        .iter()
        .filter_map(|o| match o {
            OwnerRef::Account(u) => Some(u.as_str()),
            OwnerRef::Team { .. } => None,
        })
        .collect();
    let team_slugs: Vec<&str> = owners
        .iter()
        .filter_map(|o| match o {
            OwnerRef::Team { org, team } if org == org_slug => Some(team.as_str()),
            _ => None,
        })
        .collect();

    let mut account_ids: std::collections::HashMap<String, Uuid> = std::collections::HashMap::new();
    if !usernames.is_empty() {
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT id, username FROM accounts WHERE username = ANY($1)")
                .bind(&usernames)
                .fetch_all(&mut *conn)
                .await?;
        for (id, username) in rows {
            account_ids.insert(username, id);
        }
    }

    // team slug -> member ids (a team with no members yields an empty vec).
    let mut team_members: std::collections::HashMap<String, Vec<Uuid>> =
        std::collections::HashMap::new();
    if !team_slugs.is_empty() {
        let rows: Vec<(String, Uuid)> = sqlx::query_as(
            "SELECT t.slug, tm.account_id FROM teams t \
             JOIN team_members tm ON tm.team_id = t.id \
             WHERE t.organization_id = $1 AND t.slug = ANY($2)",
        )
        .bind(org_id)
        .bind(&team_slugs)
        .fetch_all(&mut *conn)
        .await?;
        for (slug, account_id) in rows {
            team_members.entry(slug).or_default().push(account_id);
        }
        // Keep unknown teams (no rows) present so the gate treats them as
        // required-but-never-approved.
        for slug in &team_slugs {
            team_members.entry((*slug).to_owned()).or_default();
        }
    }

    Ok(owners
        .iter()
        .map(|o| match o {
            OwnerRef::Account(u) => ResolvedOwner {
                name: o.display(),
                account_ids: account_ids.get(u).copied().into_iter().collect(),
            },
            OwnerRef::Team { org, .. } if org != org_slug => ResolvedOwner {
                name: o.display(),
                account_ids: Vec::new(),
            },
            OwnerRef::Team { team, .. } => ResolvedOwner {
                name: o.display(),
                account_ids: team_members.get(team).cloned().unwrap_or_default(),
            },
        })
        .collect())
}

/// Computes the code-owner requirements for a pull request at `head` against
/// `base`: the changed files' owners and whether every changed file with
/// owners already has an approving review from one of them.
pub(crate) async fn compute_review(
    conn: &mut PgConnection,
    org_id: Uuid,
    org_slug: &str,
    pull_id: Uuid,
    head: &str,
    rules: &[Rule],
    paths: &[String],
) -> Result<CodeOwnerReview, sqlx::Error> {
    let approvers = approving_reviewers(conn, pull_id, head).await?;

    // Collect the unique owners (in first-appearance order) across every
    // changed file that has owners, then resolve them once.
    let mut unique: Vec<OwnerRef> = Vec::new();
    for path in paths {
        for owner in owners_for(rules, path) {
            if !unique.contains(&owner) {
                unique.push(owner);
            }
        }
    }
    let resolved = resolve_owners(conn, org_id, org_slug, &unique).await?;
    let resolved_by_ref: std::collections::HashMap<OwnerRef, &ResolvedOwner> =
        unique.iter().cloned().zip(resolved.iter()).collect();

    let mut required: Vec<RequiredOwner> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut satisfied = true;
    for path in paths {
        let owners = owners_for(rules, path);
        if owners.is_empty() {
            continue;
        }
        let file_satisfied = owners.iter().any(|owner| {
            resolved_by_ref
                .get(owner)
                .is_some_and(|r| r.account_ids.iter().any(|id| approvers.contains(id)))
        });
        if !file_satisfied {
            satisfied = false;
        }
        for owner in &owners {
            if let Some(r) = resolved_by_ref.get(owner)
                && seen.insert(r.name.clone())
            {
                required.push(RequiredOwner {
                    name: r.name.clone(),
                    approved: r.account_ids.iter().any(|id| approvers.contains(id)),
                });
            }
        }
    }
    Ok(CodeOwnerReview {
        required,
        satisfied,
        errors: Vec::new(),
    })
}

/// The full review status used by the pull request page and the merge gate.
pub(crate) async fn review(
    state: &AppState,
    repo_id: Uuid,
    org_id: Uuid,
    org_slug: &str,
    pull_id: Uuid,
    head: &str,
    base: &str,
) -> Result<CodeOwnerReview, ApiError> {
    let loaded = load(state, repo_id, base, head).await?;
    // `changed_files` reports `None` when base or head does not resolve, and a
    // truncated diff or an unreadable CODEOWNERS file leaves the changed-file
    // set unknown, so the gate must fail closed.
    let Some(paths) = loaded.paths else {
        return Ok(CodeOwnerReview {
            required: Vec::new(),
            satisfied: false,
            errors: loaded.errors,
        });
    };
    if loaded.diff_truncated || loaded.unreadable {
        return Ok(CodeOwnerReview {
            required: Vec::new(),
            satisfied: false,
            errors: loaded.errors,
        });
    }
    let mut conn = state.pool.acquire().await?;
    let mut review = compute_review(
        &mut conn,
        org_id,
        org_slug,
        pull_id,
        head,
        &loaded.rules,
        &paths,
    )
    .await?;
    review.errors = loaded.errors;
    Ok(review)
}

// ---------------------------------------------------------------------------
// Review-request job
// ---------------------------------------------------------------------------

pub fn register(registry: &mut jobs::Registry) {
    registry.register("codeowners.request_reviews", request_reviews_handler());
}

fn request_reviews_handler() -> jobs::Handler {
    std::sync::Arc::new(|state: AppState, job: jobs::Job| {
        Box::pin(async move { request_reviews(&state, &job).await })
    })
}

fn payload_uuid(payload: &serde_json::Value, key: &str) -> Result<Uuid, jobs::JobOutcome> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or_else(|| jobs::JobOutcome::Failed {
            reason: format!("missing or invalid `{key}` in payload"),
        })
}

/// The pull request fields the job needs, plus its repository id.
#[derive(sqlx::FromRow)]
struct JobPull {
    id: Uuid,
    repository_id: Uuid,
    source_repository_id: Uuid,
    author_id: Uuid,
    source_branch: String,
    target_branch: String,
    state: String,
    draft: bool,
}

async fn request_reviews(state: &AppState, job: &jobs::Job) -> jobs::JobOutcome {
    let pull_id = match payload_uuid(&job.payload, "pull_request_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };

    // A deleted or closed pull request needs no code-owner requests; a draft is
    // not ready for review, so its owners are not requested yet either.
    let pull = match sqlx::query_as::<_, JobPull>(
        "SELECT id, repository_id, source_repository_id, author_id, source_branch, target_branch, state, draft \
         FROM pull_requests WHERE id = $1",
    )
    .bind(pull_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(pull)) => pull,
        Ok(None) => return jobs::JobOutcome::Done,
        Err(_) => return retry(),
    };
    if pull.state != "open" || pull.draft {
        return jobs::JobOutcome::Done;
    }

    let mut conn = match state.pool.acquire().await {
        Ok(conn) => conn,
        Err(_) => return retry(),
    };
    let repo = match repositories::get(&mut conn, pull.repository_id).await {
        Ok(Some(repo)) => repo,
        Ok(None) => return jobs::JobOutcome::Done,
        Err(_) => return retry(),
    };
    let org_id = repo.organization_id;
    let org_slug: String = match sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&state.pool)
        .await
    {
        Ok(slug) => slug,
        Err(_) => return retry(),
    };

    let head = match pulls::branch_tip(state, pull.source_repository_id, &pull.source_branch).await
    {
        Ok(Some(head)) => head,
        Ok(None) => return jobs::JobOutcome::Done,
        Err(_) => return retry(),
    };
    let base = match pulls::branch_tip(state, pull.repository_id, &pull.target_branch).await {
        Ok(Some(base)) => base,
        Ok(None) => return jobs::JobOutcome::Done,
        Err(_) => return retry(),
    };

    // A cross-repository pull takes its head from a fork; bring its objects
    // into the target so the changed-file diff below can resolve the head there
    // (ADR 0008). Same-repository pulls already have the head.
    if pulls::fetch_cross_repo_head(
        state,
        pull.repository_id,
        pull.source_repository_id,
        &pull.source_branch,
    )
    .await
    .is_err()
    {
        return retry();
    }

    let loaded = match load(state, pull.repository_id, &base, &head).await {
        Ok(v) => v,
        Err(_) => return retry(),
    };
    // An unresolvable base/head leaves the changed-file set unknown, so there
    // are no owners to request reviews from.
    let paths = loaded.paths.unwrap_or_default();

    let account_ids = match reviewer_accounts(state, org_id, &org_slug, &loaded.rules, &paths).await
    {
        Ok(ids) => ids,
        Err(()) => return retry(),
    };

    let pull_json = pulls::PullJson {
        id: pull.id,
        number: 0,
        title: String::new(),
        body: String::new(),
        state: pull.state.clone(),
        draft: pull.draft,
        author_id: pull.author_id,
        source_branch: pull.source_branch.clone(),
        target_branch: pull.target_branch.clone(),
        source_repository_id: pull.source_repository_id,
        closed_reason: None,
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
    };

    for reviewer_id in account_ids {
        if reviewer_id == pull.author_id {
            continue;
        }
        let facts = match crate::facts::load(&mut conn, reviewer_id, org_id).await {
            Ok(Some(facts)) => facts,
            Ok(None) => continue,
            Err(_) => return retry(),
        };
        if !pulls::reviewer_can_approve(
            githearth_authz::Action::PullRequestApprove,
            state,
            &repo,
            &pull_json,
            &facts,
        ) {
            continue;
        }
        drop(conn);

        let mut tx = match state.pool.begin().await {
            Ok(tx) => tx,
            Err(_) => return retry(),
        };
        match pulls::insert_review_request(&mut tx, org_id, pull.id, reviewer_id, pull.author_id)
            .await
        {
            Ok(_) => {
                if tx.commit().await.is_err() {
                    return retry();
                }
            }
            Err(_) => return retry(),
        }

        conn = match state.pool.acquire().await {
            Ok(conn) => conn,
            Err(_) => return retry(),
        };
    }

    jobs::JobOutcome::Done
}

/// The account ids of every owner of the changed files, deduplicated.
async fn reviewer_accounts(
    state: &AppState,
    org_id: Uuid,
    org_slug: &str,
    rules: &[Rule],
    paths: &[String],
) -> Result<Vec<Uuid>, ()> {
    let mut unique: Vec<OwnerRef> = Vec::new();
    for path in paths {
        for owner in owners_for(rules, path) {
            if !unique.contains(&owner) {
                unique.push(owner);
            }
        }
    }
    let mut conn = state.pool.acquire().await.map_err(|_| ())?;
    let resolved = resolve_owners(&mut conn, org_id, org_slug, &unique)
        .await
        .map_err(|_| ())?;
    let mut ids: Vec<Uuid> = Vec::new();
    for r in resolved {
        for id in r.account_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

fn retry() -> jobs::JobOutcome {
    jobs::JobOutcome::Retry {
        after: Duration::ZERO,
        reason: "database or git error".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn run_git(args: &[&str]) -> String {
        let mut command = StdCommand::new("git");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .args(args);
        let out = command.output().expect("git");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Creates a repository with the given files committed on `main` and
    /// returns `(work tree, git dir)`.
    fn make_repo(files: &[(&str, &[u8])]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("codeowners-blob-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        run_git(&["init", "-q", "-b", "main", d]);
        for (path, content) in files {
            if let Some(parent) = Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(dir.join(parent)).unwrap();
            }
            std::fs::write(dir.join(path), content).unwrap();
        }
        run_git(&["-C", d, "add", "-A"]);
        run_git(&[
            "-C",
            d,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "root",
        ]);
        (dir.clone(), dir.join(".git"))
    }

    #[test]
    fn read_codeowners_blob_resolves_readable_and_missing_files() {
        let (dir, repo) = make_repo(&[("CODEOWNERS", b"*.rs @alice\n")]);
        assert!(matches!(
            read_codeowners_blob(&repo, "HEAD", Duration::from_secs(10)).unwrap(),
            CodeOwnersBlob::Readable(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let (dir, repo) = make_repo(&[("README.md", b"hello\n")]);
        assert!(matches!(
            read_codeowners_blob(&repo, "HEAD", Duration::from_secs(10)).unwrap(),
            CodeOwnersBlob::Missing
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_codeowners_blob_is_authoritative_once_a_path_resolves() {
        // A later path with valid content must not mask an earlier file that is
        // unreadable: once CODEOWNERS resolves it is authoritative.
        for content in [b"*.rs @alice\0".to_vec(), b"*.rs @alice\xff\n".to_vec()] {
            let (dir, repo) = make_repo(&[
                ("CODEOWNERS", &content),
                ("docs/CODEOWNERS", b"*.rs @bob\n"),
            ]);
            assert!(
                matches!(
                    read_codeowners_blob(&repo, "HEAD", Duration::from_secs(10)).unwrap(),
                    CodeOwnersBlob::Unreadable(_)
                ),
                "an unreadable CODEOWNERS must not fall back to docs/CODEOWNERS"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        // An oversized CODEOWNERS is authoritative too.
        let oversized = vec![b'a'; browse::MAX_BLOB_BYTES + 1];
        let (dir, repo) = make_repo(&[("CODEOWNERS", &oversized)]);
        assert!(matches!(
            read_codeowners_blob(&repo, "HEAD", Duration::from_secs(10)).unwrap(),
            CodeOwnersBlob::Unreadable(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parsing_skips_comments_and_blanks_and_reads_owners() {
        let (rules, errors) = parse(
            "# comment\n\
             \n\
             *.rs   @alice @acme/eng\n\
             docs/ @bob\n\
             # another comment\n\
             \n",
        );
        assert!(errors.is_empty());
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].pattern, "*.rs");
        assert_eq!(
            rules[0].owners,
            vec![
                OwnerRef::Account("alice".into()),
                OwnerRef::Team {
                    org: "acme".into(),
                    team: "eng".into()
                },
            ]
        );
        assert_eq!(rules[1].pattern, "docs/");
        assert_eq!(rules[1].owners, vec![OwnerRef::Account("bob".into())]);
    }

    #[test]
    fn owner_tokens_require_at_and_split_org_team() {
        assert_eq!(parse_owner("@alice"), Ok(OwnerRef::Account("alice".into())));
        assert!(parse_owner("alice").is_err());
        assert_eq!(
            parse_owner("@acme/eng"),
            Ok(OwnerRef::Team {
                org: "acme".into(),
                team: "eng".into()
            })
        );
    }

    #[test]
    fn malformed_owner_tokens_are_reported_errors() {
        for token in ["alice", "@", "@acme/", "@acme/a/b"] {
            assert!(
                parse_owner(token).is_err(),
                "token {token:?} should be malformed"
            );
        }
        let (rules, errors) = parse("*.rs @alice alice @ @acme/ @acme/a/b\n");
        assert_eq!(errors.len(), 4);
        assert_eq!(rules[0].owners, vec![OwnerRef::Account("alice".into())]);
    }

    #[test]
    fn escaped_spaces_and_hashes_survive_tokenization() {
        let (rules, errors) = parse("src/my\\ file.rs @alice\n");
        assert!(errors.is_empty());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pattern, "src/my file.rs");
    }

    #[test]
    fn last_matching_rule_wins() {
        let (rules, _) = parse(
            "*.rs @alice\n\
             src/ @bob\n",
        );
        assert_eq!(
            owners_for(&rules, "src/main.rs"),
            vec![OwnerRef::Account("bob".into())]
        );
        assert_eq!(
            owners_for(&rules, "lib.rs"),
            vec![OwnerRef::Account("alice".into())]
        );
        assert!(owners_for(&rules, "README.md").is_empty());
    }

    #[test]
    fn pattern_matching_covers_the_common_gitignore_shapes() {
        let cases = [
            ("*.rs", "src/main.rs", true),
            ("*.rs", "main.rs", true),
            ("*.rs", "src/main.js", false),
            ("docs/*", "docs/a.md", true),
            ("docs/*", "docs/sub/a.md", false),
            ("docs/**", "docs/sub/a.md", true),
            ("docs/**", "docs/a.md", true),
            ("docs/", "docs/a.md", true),
            ("docs/", "docs/a/b.md", true),
            ("docs/", "docs", false),
            ("docs/", "other/a.md", false),
            ("/root.txt", "root.txt", true),
            ("/root.txt", "sub/root.txt", false),
            ("root.txt", "sub/root.txt", true),
            ("src/foo.rs", "src/foo.rs", true),
            ("src/foo.rs", "src/bar/foo.rs", false),
            ("[ab]c.txt", "ac.txt", true),
            ("[ab]c.txt", "bc.txt", true),
            ("[ab]c.txt", "cc.txt", false),
            ("?.rs", "a.rs", true),
            ("?.rs", "ab.rs", false),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(
                pattern_matches(pattern, path),
                expected,
                "pattern {pattern:?} vs path {path:?}"
            );
        }
    }

    #[test]
    fn double_star_spans_directories() {
        assert!(pattern_matches("a/**/b", "a/x/b"));
        assert!(pattern_matches("a/**/b", "a/x/y/b"));
        assert!(pattern_matches("a/**/b", "a/b"));
        assert!(!pattern_matches("a/**/b", "a/x/c"));
    }

    #[test]
    fn wildcard_segments_do_not_cross_slashes() {
        assert!(match_glob_segment("f*.rs", "foo.rs"));
        assert!(match_glob_segment("f?.rs", "fo.rs"));
        assert!(!match_glob_segment("f*.rs", "x/foo.rs"));
    }

    #[tokio::test]
    async fn review_fails_closed_when_changed_files_are_unresolvable() {
        use githearth_authz::Engine;
        use githearth_git::layout::{ObjectFormat, Storage};
        use sqlx::postgres::PgPoolOptions;
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        let root = std::env::temp_dir().join(format!("codeowners-fail-closed-{}", Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        let repo_id = Uuid::new_v4();
        storage
            .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
            .unwrap();

        // The unresolvable revisions short-circuit before any database use, so
        // a lazy pool that never connects is enough.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let state = AppState {
            pool,
            config: crate::config::Config {
                storage_root: root.clone(),
                database_url: "postgres://localhost/unused".into(),
                ..crate::config::Config::default()
            },
            auth_slots: Arc::new(Semaphore::new(2)),
            git_slots: Arc::new(Semaphore::new(2)),
            dummy_hash: Arc::new(String::new()),
            authz: Arc::new(Engine::new().unwrap()),
            storage: Arc::new(storage),
            hook_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            hook_socket_path: std::path::PathBuf::from("/nonexistent/hook.sock"),
            trusted_proxies: Vec::new(),
        };

        let result = review(
            &state,
            repo_id,
            Uuid::new_v4(),
            "org",
            Uuid::new_v4(),
            "missing-head",
            "missing-base",
        )
        .await
        .unwrap();
        assert!(!result.satisfied);
        assert!(result.required.is_empty());
        assert!(result.errors.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}
