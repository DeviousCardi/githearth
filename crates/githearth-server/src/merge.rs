//! Merge-record replay and `execute_intent` (ADR 0004 "Execution", "Duplicate
//! delivery and concurrent workers", "Lost metadata after the Git commit").

use githearth_git::journal::Record;
use githearth_git::merge::{Candidate, Identity, Method, Request};
use sqlx::PgConnection;
use std::fmt;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

use crate::{AppState, pulls, pushes::ReplayError};

/// Applies a committed merge journal record to PostgreSQL metadata.
///
/// Person A's `record_merge_result` (ADR 0004) is idempotent per journal
/// sequence, so applying the same record twice changes nothing. A merge record
/// journals exactly one ref update; anything else is invalid.
pub(crate) async fn apply_merge_record(
    conn: &mut PgConnection,
    repository: Uuid,
    prepared: &Record,
) -> Result<(), ReplayError> {
    let [update] = prepared.updates.as_slice() else {
        return Err(ReplayError::Invalid(
            "merge record must contain exactly one ref update",
        ));
    };
    let actor = prepared
        .actor
        .ok_or(ReplayError::Invalid("merge record actor is required"))?;
    crate::merge_intents::record_merge_result(
        conn,
        repository,
        prepared.sequence,
        prepared.operation,
        actor,
        &update.old,
        &update.new,
        &update.reference,
    )
    .await?;
    Ok(())
}

/// The business outcome of an intent execution. Every possibility is a
/// variant; only infrastructure failures use [`MergeError`].
#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        oid: String,
    },
    AlreadyMerged {
        oid: String,
    },
    Conflicted {
        paths: Vec<String>,
    },
    Stale {
        reason: String,
    },
    /// A merge-queue intent's target moved; the worker re-targets it and
    /// retries instead of failing it.
    Retarget {
        new_target: String,
    },
    Failed {
        reason: String,
    },
}

/// Infrastructure failures only: database, storage and join errors. Every
/// business outcome is a [`MergeOutcome`].
#[derive(Debug)]
pub enum MergeError {
    Database(sqlx::Error),
    Storage(githearth_git::StorageError),
    Replay(ReplayError),
    Join(tokio::task::JoinError),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MergeError::Database(e) => write!(f, "database error: {e}"),
            MergeError::Storage(e) => write!(f, "storage error: {e}"),
            MergeError::Replay(e) => write!(f, "journal replay error: {e}"),
            MergeError::Join(e) => write!(f, "blocking task failed: {e}"),
        }
    }
}

impl std::error::Error for MergeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MergeError::Database(e) => Some(e),
            MergeError::Storage(e) => Some(e),
            MergeError::Replay(e) => Some(e),
            MergeError::Join(e) => Some(e),
        }
    }
}

impl From<sqlx::Error> for MergeError {
    fn from(e: sqlx::Error) -> Self {
        MergeError::Database(e)
    }
}

impl From<githearth_git::StorageError> for MergeError {
    fn from(e: githearth_git::StorageError) -> Self {
        MergeError::Storage(e)
    }
}

impl From<ReplayError> for MergeError {
    fn from(e: ReplayError) -> Self {
        MergeError::Replay(e)
    }
}

impl From<tokio::task::JoinError> for MergeError {
    fn from(e: tokio::task::JoinError) -> Self {
        MergeError::Join(e)
    }
}

#[derive(sqlx::FromRow)]
struct Intent {
    pull_request_id: Uuid,
    source_ref: String,
    source_oid: String,
    target_ref: String,
    expected_target_oid: String,
    method: String,
    candidate_oid: Option<String>,
    policy_generation: i64,
    checks_approvals_digest: String,
    actor_id: Uuid,
    state: String,
    in_queue: bool,
    result_oid: Option<String>,
}

const INTENT_FIELDS: &str = "pull_request_id, source_ref, source_oid, \
 target_ref, expected_target_oid, method, candidate_oid, policy_generation, \
 checks_approvals_digest, actor_id, state, in_queue, result_oid";

async fn load_intent(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
) -> Result<Option<Intent>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {INTENT_FIELDS} FROM merge_intents WHERE operation_id = $1"
    ))
    .bind(operation_id)
    .fetch_optional(pool)
    .await
}

/// Resolves `rev` to a full commit id on the blocking pool.
async fn read_ref(repo: &Path, rev: &str, timeout: Duration) -> Result<Option<String>, MergeError> {
    let repo = repo.to_path_buf();
    let rev = rev.to_owned();
    tokio::task::spawn_blocking(move || {
        githearth_git::browse::resolve(&repo, &rev, timeout).map_err(MergeError::Storage)
    })
    .await?
}

/// Executes a merge intent whose state is `executing`, exactly per ADR 0004
/// "Execution" and "Duplicate delivery and concurrent workers".
pub async fn execute_intent(
    state: &AppState,
    operation_id: Uuid,
) -> Result<MergeOutcome, MergeError> {
    // 1. Read the repository id and the pull's source repository id before any
    // lock, so every lock below is acquired before a pool connection.
    let (repository_id, source_repository_id): (Uuid, Uuid) = {
        let mut conn = state.pool.acquire().await?;
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT mi.repository_id, pr.source_repository_id \
             FROM merge_intents mi JOIN pull_requests pr ON pr.id = mi.pull_request_id \
             WHERE mi.operation_id = $1",
        )
        .bind(operation_id)
        .fetch_optional(&mut *conn)
        .await?;
        match row {
            Some(row) => row,
            None => {
                return Ok(MergeOutcome::Failed {
                    reason: "merge intent not found".into(),
                });
            }
        }
    };

    // 2. Serialize every ref writer on the source fork (if any) and then on
    // this repository. A cross-repository merge reads and fetches the fork's
    // head, so it must exclude a concurrent push to or deletion of the fork.
    // The lock order matches fork creation (fork before parent); the target
    // lock is held to the end.
    let cross_repo = source_repository_id != repository_id;
    let _source_guard = if cross_repo {
        Some(
            state
                .storage
                .repository_lock(source_repository_id)
                .lock_owned()
                .await,
        )
    } else {
        None
    };
    let _guard = state
        .storage
        .repository_lock(repository_id)
        .lock_owned()
        .await;

    let repo_path = state.storage.repository_path(repository_id);
    let hooks_dir = state.config.storage_root.join("hooks");
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);

    // 3. Journal lookup: apply any committed-but-unapplied record first (a
    // crash, or an earlier worker), then re-read the intent.
    crate::pushes::replay(state, repository_id).await?;
    let intent = match load_intent(&state.pool, operation_id).await? {
        Some(intent) => intent,
        None => {
            return Ok(MergeOutcome::Failed {
                reason: "merge intent not found".into(),
            });
        }
    };
    if intent.state == "merged" {
        return Ok(MergeOutcome::AlreadyMerged {
            oid: intent.result_oid.unwrap_or_default(),
        });
    }

    // 4. Only A's worker, which moved the intent to `executing`, may run it.
    if intent.state != "executing" {
        return Ok(MergeOutcome::Stale {
            reason: format!("intent is {}", intent.state),
        });
    }

    // 5. Revalidate every fact the intent recorded. The intent row is never
    // written here except through `record_merge_result`.
    let (pull, rules, approvals, checks_passed) = {
        let mut conn = state.pool.acquire().await?;
        let Some(pull) = pulls::fetch_pull(&mut conn, intent.pull_request_id).await? else {
            return Ok(MergeOutcome::Stale {
                reason: "pull request not found".into(),
            });
        };
        if pull.state != "open" {
            return Ok(MergeOutcome::Stale {
                reason: "pull request not open".into(),
            });
        }
        if pull.source_branch != intent.source_ref {
            return Ok(MergeOutcome::Stale {
                reason: "source branch changed".into(),
            });
        }
        if pull.target_branch != intent.target_ref {
            return Ok(MergeOutcome::Stale {
                reason: "target branch changed".into(),
            });
        }

        let (rules, generation) =
            crate::protection::load_rules_with_generation(&mut conn, repository_id).await?;
        if generation != intent.policy_generation {
            return Ok(MergeOutcome::Stale {
                reason: "branch protection changed".into(),
            });
        }

        let review_ids = crate::merge_intents::latest_review_ids(
            &mut conn,
            intent.pull_request_id,
            &intent.source_oid,
        )
        .await?;
        // Required checks are re-read at the intent's exact source oid, so a
        // status posted for any other commit cannot satisfy them, and a status
        // changed since the intent was recorded makes the digest stale.
        let required_checks = crate::protection::required_status_checks(
            &rules,
            &format!("refs/heads/{}", intent.target_ref),
        );
        let check_states = crate::statuses::latest_required_states(
            &mut conn,
            repository_id,
            &intent.source_oid,
            &required_checks,
        )
        .await?;
        let digest = crate::merge_intents::checks_approvals_digest(
            &intent.source_oid,
            &review_ids,
            &check_states,
        );
        if digest != intent.checks_approvals_digest {
            return Ok(MergeOutcome::Stale {
                reason: "approvals or checks changed".into(),
            });
        }

        let approvals = crate::merge_intents::approvals_at(
            &mut conn,
            intent.pull_request_id,
            &intent.source_oid,
        )
        .await? as u32;
        let checks_passed = crate::statuses::all_success(&required_checks, &check_states);

        (pull, rules, approvals, checks_passed)
    };

    let target_ref = format!("refs/heads/{}", intent.target_ref);
    let source_ref = format!("refs/heads/{}", intent.source_ref);
    let current_target = read_ref(&repo_path, &target_ref, timeout).await?;
    if current_target.as_deref() != Some(intent.expected_target_oid.as_str()) {
        // A merge-queue entry whose target was moved by an earlier entry is
        // re-targeted to the current tip and retried; a direct merge stays
        // terminal (`stale`) so the enabler can re-create it deliberately.
        if intent.in_queue
            && let Some(new_target) = current_target
        {
            return Ok(MergeOutcome::Retarget { new_target });
        }
        return Ok(MergeOutcome::Stale {
            reason: "target moved".into(),
        });
    }

    // A cross-repository pull takes its head from a fork. Read the head from
    // the fork (which must still be active) and bring its objects into the
    // target so the candidate can be built there (ADR 0008). The target's
    // per-repository lock and the fork's lock are already held (step 2); the
    // fork is only read, never written.
    let source_repo_path = if cross_repo {
        let mut conn = state.pool.acquire().await?;
        let Some(source_repo) = crate::repositories::get(&mut conn, source_repository_id).await?
        else {
            return Ok(MergeOutcome::Stale {
                reason: "source repository not found".into(),
            });
        };
        if source_repo.status != githearth_core::RepositoryStatus::Active {
            return Ok(MergeOutcome::Stale {
                reason: "source repository not active".into(),
            });
        }
        state.storage.repository_path(source_repository_id)
    } else {
        repo_path.clone()
    };

    if read_ref(&source_repo_path, &source_ref, timeout)
        .await?
        .as_deref()
        != Some(intent.source_oid.as_str())
    {
        return Ok(MergeOutcome::Stale {
            reason: "source moved".into(),
        });
    }

    if cross_repo {
        let storage = state.storage.clone();
        let fetch_ref = source_ref.clone();
        tokio::task::spawn_blocking(move || {
            storage.fetch_ref(repository_id, source_repository_id, &fetch_ref)
        })
        .await
        .map_err(MergeError::Join)??;
    }

    // Actor authorization, re-evaluated under the lock. A merge is a routine
    // action, so session age does not apply.
    let (actor_username, pr_author_username) = {
        let mut conn = state.pool.acquire().await?;
        let Some(repo) = crate::repositories::get(&mut conn, repository_id).await? else {
            return Ok(MergeOutcome::Stale {
                reason: "repository not found".into(),
            });
        };
        // A repository deleted (or still being created) after the intent was
        // queued must not have its ref moved; the API refuses it the same way.
        if repo.status != githearth_core::RepositoryStatus::Active {
            return Ok(MergeOutcome::Stale {
                reason: "repository not active".into(),
            });
        }
        let Some(facts) =
            crate::facts::load(&mut conn, intent.actor_id, repo.organization_id).await?
        else {
            return Ok(MergeOutcome::Stale {
                reason: "actor no longer authorized".into(),
            });
        };
        let protection = crate::protection::matching(&rules, &target_ref);
        let org_slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
            .bind(repo.organization_id)
            .fetch_one(&mut *conn)
            .await?;
        let code_owner_review_ok = crate::codeowners::review(
            state,
            repository_id,
            repo.organization_id,
            &org_slug,
            pull.id,
            &intent.source_oid,
            &intent.expected_target_oid,
        )
        .await
        .map(|r| r.satisfied)
        .unwrap_or(false);
        if !crate::merge_intents::merge_allowed(
            state,
            &facts,
            &repo,
            &pull,
            protection,
            approvals,
            checks_passed,
            code_owner_review_ok,
            None,
        ) {
            return Ok(MergeOutcome::Stale {
                reason: "actor no longer authorized".into(),
            });
        }
        let actor_username: String =
            sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
                .bind(intent.actor_id)
                .fetch_one(&mut *conn)
                .await?;
        let pr_author_username: String =
            sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
                .bind(pull.author_id)
                .fetch_one(&mut *conn)
                .await?;
        (actor_username, pr_author_username)
    };

    // 6. Build (or reuse) the candidate.
    let method = match intent.method.as_str() {
        "merge" => Method::Merge,
        "squash" => Method::Squash,
        "rebase" => Method::Rebase,
        "ff_only" => Method::FastForward,
        _ => {
            return Ok(MergeOutcome::Failed {
                reason: "unknown merge method".into(),
            });
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let committer = Identity {
        name: "GitHearth".into(),
        email: "noreply@githearth.invalid".into(),
        time: now,
        offset: "+0000".into(),
    };
    let (message, author) = match method {
        Method::Squash => {
            let mut message = pull.title.clone();
            if !pull.body.trim().is_empty() {
                message.push_str("\n\n");
                message.push_str(pull.body.trim());
            }
            message.push_str("\n\nMerged-by: ");
            message.push_str(&actor_username);
            (
                message,
                Identity {
                    name: pr_author_username.clone(),
                    email: format!("{pr_author_username}@users.noreply.githearth.invalid"),
                    time: now,
                    offset: "+0000".into(),
                },
            )
        }
        _ => (
            format!(
                "Merge pull request #{} from {}",
                pull.number, pull.source_branch
            ),
            Identity {
                name: actor_username.clone(),
                email: format!("{actor_username}@users.noreply.githearth.invalid"),
                time: now,
                offset: "+0000".into(),
            },
        ),
    };

    let reused = match intent.candidate_oid.as_deref() {
        Some(candidate) => {
            let first_parent = read_ref(&repo_path, &format!("{candidate}^1"), timeout).await?;
            (first_parent.as_deref() == Some(intent.expected_target_oid.as_str()))
                .then(|| candidate.to_owned())
        }
        None => None,
    };

    let new_oid = match reused {
        Some(oid) => oid,
        None => {
            let target = intent.expected_target_oid.clone();
            let source = intent.source_oid.clone();
            let message_owned = message.clone();
            let author_owned = author.clone();
            let committer_owned = committer.clone();
            let repo = repo_path.clone();
            let candidate = tokio::task::spawn_blocking(move || {
                let request = Request {
                    method,
                    target: &target,
                    source: &source,
                    message: &message_owned,
                    author: &author_owned,
                    committer: &committer_owned,
                };
                githearth_git::merge::build_candidate(&repo, None, &request, timeout)
            })
            .await?;
            match candidate? {
                Candidate::Ready { oid } => oid,
                Candidate::Conflicts(paths) => return Ok(MergeOutcome::Conflicted { paths }),
                Candidate::UpToDate => {
                    return Ok(MergeOutcome::Failed {
                        reason: "nothing to merge".into(),
                    });
                }
                Candidate::NotFastForward => {
                    return Ok(MergeOutcome::Stale {
                        reason: "target is not an ancestor of the source".into(),
                    });
                }
                Candidate::Unrelated => {
                    return Ok(MergeOutcome::Failed {
                        reason: "branches have no common history".into(),
                    });
                }
                Candidate::RebaseMergeCommit => {
                    return Ok(MergeOutcome::Failed {
                        reason: "cannot rebase merge commits".into(),
                    });
                }
            }
        }
    };

    // 7. Compare-and-swap the target ref through the journaling hook.
    let repo = repo_path.clone();
    let target_ref_for_update = target_ref.clone();
    let expected_old = intent.expected_target_oid.clone();
    let new_oid_for_update = new_oid.clone();
    let actor_id = intent.actor_id;
    let update = tokio::task::spawn_blocking(move || {
        githearth_git::merge::update_ref_expected(
            &repo,
            &hooks_dir,
            &target_ref_for_update,
            &new_oid_for_update,
            &expected_old,
            operation_id,
            actor_id,
            githearth_git::journal::Origin::Merge,
            timeout,
        )
    })
    .await?;
    match update {
        Ok(()) => {}
        Err(githearth_git::StorageError::RefMoved(_)) => {
            return Ok(MergeOutcome::Stale {
                reason: "target moved".into(),
            });
        }
        Err(error) => return Err(MergeError::Storage(error)),
    }

    // 8. Apply the journaled merge to metadata. A replay failure after the ref
    // moved never turns a merge into a failure; the reconciler converges it.
    if let Err(error) = crate::pushes::replay(state, repository_id).await {
        tracing::warn!(
            repository_id = %repository_id,
            operation_id = %operation_id,
            error = %error,
            "post-merge journal replay failed; reconciler will retry"
        );
    }

    Ok(MergeOutcome::Merged { oid: new_oid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn merge_error_display_source_and_from() {
        let db = MergeError::Database(sqlx::Error::RowNotFound);
        assert!(db.to_string().contains("database error"));
        assert!(db.source().is_some());

        let storage = MergeError::Storage(githearth_git::StorageError::Invalid("boom".into()));
        assert!(storage.to_string().contains("storage error"));
        assert!(storage.source().is_some());

        let replay = MergeError::Replay(ReplayError::Invalid("m"));
        assert!(replay.to_string().contains("journal replay error"));
        assert!(replay.source().is_some());

        assert!(matches!(
            MergeError::from(sqlx::Error::RowNotFound),
            MergeError::Database(_)
        ));
        assert!(matches!(
            MergeError::from(githearth_git::StorageError::Timeout),
            MergeError::Storage(_)
        ));
        assert!(matches!(
            MergeError::from(ReplayError::Invalid("m")),
            MergeError::Replay(_)
        ));
    }

    #[tokio::test]
    async fn merge_error_join_display_source_and_from() {
        let join = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let merged = MergeError::from(join);
        assert!(matches!(merged, MergeError::Join(_)));
        assert!(merged.to_string().contains("blocking task failed"));
        assert!(merged.source().is_some());
    }
}
