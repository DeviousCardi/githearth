//! The outbox handler interface (M4.1 6.0, Person A).
//!
//! `outbox_events` records one row per committed domain event, in the same
//! transaction as the change it describes, so a row exists exactly when its
//! domain change committed ([`crate::event_with`]). The relay that will claim
//! undelivered rows, dispatch them and set `delivered_at` is Person B's work
//! and is not implemented here; this module fixes only the contract the relay
//! and the domain handlers agree on:
//!
//! * **event kind → handler.** Each [`Handler`] names an `outbox_events.kind`
//!   and the `jobs.kind` that consumes it. The domain already enqueues those
//!   jobs inside its transaction today (see `pulls.rs`, `merge_intents.rs`);
//!   the mapping lets a future relay produce the same jobs from the log alone.
//! * **deduplication key.** At-least-once delivery means the same event can be
//!   dispatched more than once. Each handler derives a key that is stable for
//!   one logical event across redelivery; the job's own `dedupe_key` and the
//!   handler's side effect (for example `notifications` keyed on
//!   `(event_id, account_id)`) both use it. The outbox event id is the default
//!   because it is unique and stable (ADR 0006, "stable keys").
//!
//! Nothing in this module reads or writes `outbox_events`; it is pure mapping
//! so the relay can be reviewed and tested on its own.

use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

/// One outbox event kind, the handler job it dispatches to, and how that job
/// deduplicates.
pub struct Handler {
    /// The `outbox_events.kind` this handles.
    pub event_kind: &'static str,
    /// The `jobs.kind` the relay enqueues.
    pub job_kind: &'static str,
    /// Derives the job's dedupe key. Must return the same value for the same
    /// logical event every time it is called, so a redelivered event collides
    /// with the job already enqueued for it.
    pub dedupe_key: fn(Uuid, &Value) -> String,
}

/// The default key: the outbox event id, which is unique and stable.
fn event_id_key(event_id: Uuid, _payload: &Value) -> String {
    event_id.to_string()
}

/// A handler whose job is keyed on a business id carried in the payload, falling
/// back to the event id when the payload does not carry one.
fn payload_key(payload: &Value, key: &str, event_id: Uuid) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| event_id.to_string())
}

fn review_id_key(event_id: Uuid, payload: &Value) -> String {
    payload_key(payload, "review_id", event_id)
}

fn request_id_key(event_id: Uuid, payload: &Value) -> String {
    payload_key(payload, "request_id", event_id)
}

/// Every kind the relay dispatches, one entry per handler. A kind the domain
/// emits but that has no handler is delivered as a no-op by the relay.
pub const HANDLERS: &[Handler] = &[
    Handler {
        event_kind: "pull_request.merged",
        job_kind: "notification.pull_request_merged",
        // The domain keys the merged notification on the audit/outbox event id
        // (`merge_intents.rs`), so the event id is the stable key.
        dedupe_key: event_id_key,
    },
    Handler {
        event_kind: "pull_request.reviewed",
        job_kind: "notification.pull_request_reviewed",
        dedupe_key: review_id_key,
    },
    Handler {
        event_kind: "pull_request.approved",
        job_kind: "notification.pull_request_reviewed",
        dedupe_key: review_id_key,
    },
    Handler {
        event_kind: "pull_request.changes_requested",
        job_kind: "notification.pull_request_reviewed",
        dedupe_key: review_id_key,
    },
    Handler {
        event_kind: "pull_request.review_requested",
        job_kind: "notification.pull_request_review_requested",
        dedupe_key: request_id_key,
    },
];

/// The handler for an outbox event kind, or `None` when the kind has none.
pub fn handler_for(event_kind: &str) -> Option<&'static Handler> {
    HANDLERS.iter().find(|h| h.event_kind == event_kind)
}

/// The job a relay would enqueue for one outbox event.
pub struct PlannedJob {
    pub kind: &'static str,
    pub payload: Value,
    pub dedupe_key: String,
    pub delay: Option<Duration>,
}

/// Plans the job for one outbox event, or `None` when the kind is unmapped.
/// `payload` is the `outbox_events.payload` value, which always carries ids,
/// never content.
pub fn plan(event_id: Uuid, event_kind: &str, payload: &Value) -> Option<PlannedJob> {
    let handler = handler_for(event_kind)?;
    Some(PlannedJob {
        kind: handler.job_kind,
        payload: payload.clone(),
        dedupe_key: (handler.dedupe_key)(event_id, payload),
        delay: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_planned_job_names_a_registered_handler() {
        for handler in HANDLERS {
            assert_eq!(
                handler_for(handler.event_kind).map(|h| h.job_kind),
                Some(handler.job_kind),
                "{}",
                handler.event_kind
            );
        }
        assert!(handler_for("not.a.kind").is_none());
    }

    #[test]
    fn dedupe_keys_are_stable_for_one_event_and_use_business_ids_when_present() {
        let event = Uuid::new_v4();
        // Merged notifications key on the event id.
        let merged = plan(event, "pull_request.merged", &json!({})).unwrap();
        assert_eq!(merged.kind, "notification.pull_request_merged");
        assert_eq!(merged.dedupe_key, event.to_string());
        // A redelivery of the same event yields the same key.
        assert_eq!(
            plan(event, "pull_request.merged", &json!({"actor_id": "x"}))
                .unwrap()
                .dedupe_key,
            event.to_string()
        );
        // A review keys on its review id, so a retry of the same review collides.
        let review = Uuid::new_v4();
        let planned = plan(
            event,
            "pull_request.approved",
            &json!({"review_id": review.to_string()}),
        )
        .unwrap();
        assert_eq!(planned.dedupe_key, review.to_string());
        // Without the business id it falls back to the stable event id.
        assert_eq!(
            plan(event, "pull_request.approved", &json!({}))
                .unwrap()
                .dedupe_key,
            event.to_string()
        );
        // A review request keys on the request id.
        let request = Uuid::new_v4();
        assert_eq!(
            plan(
                event,
                "pull_request.review_requested",
                &json!({"request_id": request.to_string()})
            )
            .unwrap()
            .dedupe_key,
            request.to_string()
        );
    }

    #[test]
    fn unmapped_kinds_plan_nothing() {
        assert!(plan(Uuid::new_v4(), "repository.pushed", &json!({})).is_none());
    }
}
