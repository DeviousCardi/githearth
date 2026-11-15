//! The operator metrics endpoint (M4.1 U14).
//!
//! Off by default. When enabled it answers only a client whose resolved
//! address is loopback, on the main router, so an operator does not have to run
//! and firewall a second listener. "Resolved" is the same value the login
//! throttle and the request log use ([`crate::client_ip`]): the peer address,
//! or the rightmost untrusted `X-Forwarded-For` entry when the peer is a
//! configured trusted proxy. A non-loopback client gets a bare `404` — status,
//! empty body, no content type — not a `401` and not the HTML not-found page:
//! the endpoint does not confirm that it exists, and the refusal carries no
//! exposition.
//!
//! The output is Prometheus text exposition, written by hand. The crate is
//! already linked into the server, so the format costs no dependency and the
//! escaping rules are small enough to test directly ([`escape_label`],
//! [`metric_name`]).
//!
//! Every metric and every label key below is published in
//! `docs/operations/metrics.md`. The list is one table ([`PUBLISHED`]) and the
//! output is produced only from it, so a metric cannot be emitted without being
//! documented; a test parses that document with `include_str!` and fails when
//! the two disagree, so a metric added only to the table is caught too. No
//! label carries an organization, repository or account identifier, a name, an
//! email, an address or a token, and every label value comes from a fixed set:
//! cardinality is bounded by the list in that document, not by the size of the
//! installation.

use crate::AppState;
use crate::client_ip::ResolvedIp;
use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use sqlx::Row;
use std::{
    fmt::Write as _,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// `Content-Type` of the response, in the versioned form Prometheus expects.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The route. One path, so this page is the only place it is written.
pub const PATH: &str = "/metrics";

/// The HTTP methods the request counters label separately. A method outside
/// this list is counted as `OTHER`, so a hostile client cannot create series.
pub const METHODS: [&str; 10] = [
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT", "OTHER",
];

/// The status classes the request counters label separately. `OTHER` covers a
/// status below 100, which no HTTP client can produce but a hypothetical
/// middleware could.
pub const STATUS_CLASSES: [&str; 6] = ["1xx", "2xx", "3xx", "4xx", "5xx", "OTHER"];

/// The rack-facing Git services a completed request was for.
pub const GIT_SERVICES: [&str; 3] = ["upload_pack", "receive_pack", "info_refs"];

/// Admission, job and housekeeping outcome label values.
const OUTCOMES: [&str; 5] = ["succeeded", "refused", "deferred", "skipped", "failed"];

/// The job kinds `serve` registers. `/metrics` publishes one series per kind on
/// this list, and a row whose kind is not here is counted under `other`, so the
/// label set is fixed by the binary rather than by the database. It is checked
/// against the registry by `serve_registers_only_published_kinds`;
/// `src/main.rs` passes the registry's own list at start-up.
pub const JOB_KINDS: [&str; 6] = [
    "codeowners.request_reviews",
    "notification.outbox",
    "notification.pull_request_merged",
    "notification.pull_request_reviewed",
    "notification.pull_request_review_requested",
    "repository.housekeeping",
];

/// The `repository_housekeeping.last_outcome` values, plus `never` (the row
/// exists and has not been maintained) and `unknown` (a value this binary does
/// not name). The scheduler and the job write the first four.
const HOUSEKEEPING_OUTCOMES: [&str; 5] = ["completed", "deferred", "failed", "never", "unknown"];

/// The states `jobs.state` holds, in the order the list publishes them.
const JOB_STATES: [&str; 4] = ["pending", "running", "done", "dead"];

/// The outbox delivery states: still undelivered, or delivered.
const OUTBOX_STATES: [&str; 2] = ["undelivered", "delivered"];

/// The number of label values one metric may have. A metric whose claimed
/// label sets produce more values than this fails
/// `every_published_metric_is_named_and_bounded`, which keeps `PUBLISHED` from
/// growing an unbounded surface without anyone noticing.
pub const MAX_SERIES_PER_METRIC: usize = 128;

/// A metric's type, which fixes the exposition lines it produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        }
    }
}

/// One published metric: its name, type, help text and the label keys in the
/// order the output writes them. Label values are always from a fixed set;
/// this list is what fixes them.
pub struct MetricDef {
    pub name: &'static str,
    pub kind: Kind,
    pub help: &'static str,
    pub labels: &'static [&'static str],
}

const fn m(
    name: &'static str,
    kind: Kind,
    help: &'static str,
    labels: &'static [&'static str],
) -> MetricDef {
    MetricDef {
        name,
        kind,
        help,
        labels,
    }
}

/// Everything this endpoint may emit. `docs/operations/metrics.md` documents
/// this list; the acceptance test parses the output and fails on any metric or
/// label key that is not here.
pub const PUBLISHED: &[MetricDef] = &[
    m(
        "githearth_http_requests_total",
        Kind::Counter,
        "HTTP requests answered, by method and status class.",
        &["method", "status"],
    ),
    m(
        "githearth_http_request_duration_mean_seconds",
        Kind::Gauge,
        "Mean HTTP request duration in seconds since the server started, by method and status class. From whole-millisecond samples, like the request log, so the resolution is 0.001 s.",
        &["method", "status"],
    ),
    m(
        "githearth_http_requests_in_flight",
        Kind::Gauge,
        "HTTP requests currently being answered, including the scrape that reads this.",
        &[],
    ),
    m(
        "githearth_git_requests_total",
        Kind::Counter,
        "Completed Git Smart HTTP requests, by service and status class.",
        &["service", "status"],
    ),
    m(
        "githearth_git_admissions_total",
        Kind::Counter,
        "Git admission decisions, by outcome and the admission's reason category.",
        &["outcome", "reason"],
    ),
    m(
        "githearth_git_push_admissions_total",
        Kind::Counter,
        "Ref-update admissions requested by the push hooks, by outcome.",
        &["outcome"],
    ),
    m(
        "githearth_jobs",
        Kind::Gauge,
        "Jobs in the durable queue, by state.",
        &["state"],
    ),
    m(
        "githearth_jobs_by_kind",
        Kind::Gauge,
        "Jobs that are pending or running, by registered kind or `other`.",
        &["kind", "state"],
    ),
    m(
        "githearth_housekeeping_repositories",
        Kind::Gauge,
        "Repositories by their last housekeeping outcome.",
        &["outcome"],
    ),
    m(
        "githearth_outbox_events",
        Kind::Gauge,
        "Committed outbox events, by delivery state.",
        &["state"],
    ),
    m(
        "githearth_git_in_flight",
        Kind::Gauge,
        "Git transport requests holding a Git slot.",
        &[],
    ),
    m(
        "githearth_auth_slots_in_use",
        Kind::Gauge,
        "Sign-in checks holding an authentication slot.",
        &[],
    ),
    m(
        "githearth_housekeeping_runs_in_flight",
        Kind::Gauge,
        "Housekeeping runs holding a housekeeping slot.",
        &[],
    ),
    m(
        "githearth_metrics_scrape_failures_total",
        Kind::Counter,
        "Metric collections that could not read a database source; the gauges it could not read are reported as 0.",
        &[],
    ),
];

/// The published definition for `name`, or `None` for an unpublished metric.
pub fn definition(name: &str) -> Option<&'static MetricDef> {
    PUBLISHED.iter().find(|d| d.name == name)
}

/// Every value the `reason` label may take. `succeeded` admissions use `none`.
pub const REASONS: [&str; 16] = [
    "none",
    "invalid_credentials",
    "not_found",
    "insufficient_grant",
    "not_clonable",
    "read_only",
    "protected_ref",
    "ref_namespace",
    "throttled",
    "busy",
    "conflict",
    "bad_request",
    "too_large",
    "unavailable",
    "internal",
    // An admission message outside the fixed set (a Git error) is reported by
    // category, never echoed: a message can carry a path or an object id.
    "other",
];

/// The `reason` label an admission refusal gets, derived from its message.
/// The messages come from the fixed set in `git_admission` and `git_http`; any
/// other text (a Git error, say) is reported as `other` rather than echoed,
/// because a message can carry a path or an object id. `none` is never
/// returned: a refusal always has a reason.
pub fn admission_reason(message: &str) -> &'static str {
    let reason = if message.starts_with("protected branch:") {
        "protected_ref"
    } else if message.starts_with("permission denied:") {
        "insufficient_grant"
    } else if message == "ref namespace is read-only" {
        "ref_namespace"
    } else {
        match message {
            "invalid or expired credentials" => "invalid_credentials",
            "not found" => "not_found",
            "not permitted" => "not_clonable",
            "read-only token" => "read_only",
            "too many sign-in attempts; try again later" | "authentication busy" => "throttled",
            "git requests busy" => "busy",
            "repository needs repair" => "conflict",
            "invalid input" => "bad_request",
            "request body exceeds limit" => "too_large",
            "database unavailable" => "unavailable",
            "internal server error" => "internal",
            _ => "other",
        }
    };
    debug_assert!(
        REASONS.contains(&reason),
        "{reason} is not a published reason"
    );
    reason
}

/// The status class label for `status`.
pub fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "OTHER",
    }
}

/// The method label for `method`; anything outside the published list is
/// `OTHER`.
pub fn method_label(method: &str) -> &'static str {
    METHODS
        .iter()
        .find(|published| **published == method && **published != "OTHER")
        .copied()
        .unwrap_or("OTHER")
}

/// The Git service a request path names, or `None` when it is not a Git path.
pub fn git_service(uri_path: &str) -> Option<&'static str> {
    if uri_path.ends_with("/git-upload-pack") {
        Some("upload_pack")
    } else if uri_path.ends_with("/git-receive-pack") {
        Some("receive_pack")
    } else if uri_path.ends_with("/info/refs") {
        Some("info_refs")
    } else {
        None
    }
}

/// The `kind` label for a job kind: a registered kind's own name, or `other`.
pub fn job_kind_label(kind: &str, registered: &'static [&'static str]) -> &'static str {
    registered
        .iter()
        .find(|name| **name == kind)
        .copied()
        .unwrap_or("other")
}

/// Escapes a Prometheus label value: backslash, double quote and newline.
///
/// A label value is only ever built from the fixed sets above, so this should
/// never do work in practice; it exists because a future label value that was
/// not fixed must not be able to break the exposition or inject a line. The
/// format has no escape for another control character, so [`write_sample`]
/// refuses a value that contains one rather than writing it raw.
pub fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

/// Refuses a metric name outside `[a-zA-Z_:][a-zA-Z0-9_:]*`, which is what the
/// exposition format and Prometheus itself accept. Nothing in [`PUBLISHED`]
/// needs the check; a hand-written line elsewhere does.
pub fn metric_name(name: &str) -> Option<&str> {
    let mut bytes = name.bytes();
    let first = bytes.next()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b':') {
        return None;
    }
    if !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':') {
        return None;
    }
    Some(name)
}

/// Writes one sample line, refusing a name, a label key or a label value the
/// format cannot carry: a name or key outside
/// `[a-zA-Z_:][a-zA-Z0-9_:]*`, or a value containing a control character other
/// than a newline (which is escaped as `\n`). A carriage return, a tab or a NUL
/// in a value would split or truncate the sample line for a scraper, and the
/// format has no escape for them.
/// Returns `false` when the line was refused, so a caller cannot silently emit
/// nothing.
/// The line is built in scratch first, so a refusal leaves `out` untouched: a
/// half-written line would corrupt every sample after it.
pub fn write_sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: &str) -> bool {
    if metric_name(name).is_none()
        || labels.iter().any(|(key, _)| metric_name(key).is_none())
        || labels
            .iter()
            .any(|(_, value)| value.chars().any(|c| c.is_control() && c != '\n'))
    {
        return false;
    }
    let mut line = String::with_capacity(name.len() + 16);
    line.push_str(name);
    if !labels.is_empty() {
        line.push('{');
        for (i, (key, value)) in labels.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            line.push_str(key);
            line.push_str("=\"");
            line.push_str(&escape_label(value));
            line.push('"');
        }
        line.push('}');
    }
    line.push(' ');
    line.push_str(value);
    line.push('\n');
    out.push_str(&line);
    true
}

/// Formats one finite value: an integer without a decimal point, anything else
/// as a Prometheus float.
pub fn number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        format!("{}", value as i64)
    } else if value.is_finite() {
        format!("{value}")
    } else {
        // Not representable as a number; a sample may not be NaN by accident.
        "0".into()
    }
}

/// The label values a metric publishes for one label key, **as declared**, not
/// derived from the renderer. The renderer ([`values`]) and the tests both read
/// this, so a value a metric can write is a value declared for *that* metric:
/// `outcome="completed"` is declared on the housekeeping gauge and nowhere
/// else, and an admissions counter cannot write it even though the key is
/// published for the gauge.
fn declared_values(metric: &str, label: &str) -> Option<Vec<&'static str>> {
    let values: Vec<&'static str> = match (metric, label) {
        (_, "method") => METHODS.to_vec(),
        (_, "status") => STATUS_CLASSES.to_vec(),
        (_, "service") => GIT_SERVICES.to_vec(),
        ("githearth_housekeeping_repositories", "outcome") => HOUSEKEEPING_OUTCOMES.to_vec(),
        (_, "outcome") => OUTCOMES.to_vec(),
        (_, "reason") => REASONS.to_vec(),
        ("githearth_outbox_events", "state") => OUTBOX_STATES.to_vec(),
        ("githearth_jobs_by_kind", "state") => vec!["pending", "running"],
        (_, "state") => JOB_STATES.to_vec(),
        // The binary's registry, never the rows: a kind that is not registered
        // is counted under `other`, which is on the list for that reason.
        (_, "kind") => JOB_KINDS.iter().copied().chain(["other"]).collect(),
        _ => return None,
    };
    Some(values)
}

/// The published values of `label` on `metric`, or an empty list when that
/// metric has no such label. The sets are per metric: `outcome="completed"` is
/// published on `githearth_housekeeping_repositories` and not on an admissions
/// counter, and a value published elsewhere is still refused here.
pub fn published_values(metric: &str, label: &str) -> Vec<String> {
    let Some(def) = definition(metric) else {
        return Vec::new();
    };
    if !def.labels.contains(&label) {
        return Vec::new();
    }
    declared_values(metric, label)
        .expect("every published label has a declared value set")
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The published values of `label` on every published metric that carries it,
/// in publication order without duplicates. This is the whole vocabulary the
/// document lists for a key; per-metric membership is [`published_values`].
pub fn published_values_anywhere(label: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for def in PUBLISHED {
        for value in published_values(def.name, label) {
            if !all.contains(&value) {
                all.push(value);
            }
        }
    }
    all
}

/// How many label-value combinations one published metric claims.
pub fn label_values(def: &MetricDef) -> usize {
    def.labels
        .iter()
        .map(|label| label_cardinality(label))
        .product()
}

/// The size of a label key's value set.
pub fn label_cardinality(label: &str) -> usize {
    match label {
        "method" => METHODS.len(),
        "status" => STATUS_CLASSES.len(),
        "service" => GIT_SERVICES.len(),
        "outcome" => OUTCOMES.len().max(HOUSEKEEPING_OUTCOMES.len()),
        "reason" => REASONS.len(),
        // The largest `state` set (jobs) bounds the others.
        "state" => JOB_STATES.len().max(OUTBOX_STATES.len()),
        "kind" => JOB_KINDS.len() + 1,
        _ => usize::MAX,
    }
}

// ---------------------------------------------------------------------------
// In-process measurements
// ---------------------------------------------------------------------------

/// The method/status-class cells an HTTP counter keeps. `None` for a method
/// outside the published list, which the middleware maps to `OTHER`.
fn index_of<T: PartialEq>(values: &[T], needle: &T) -> Option<usize> {
    values.iter().position(|v| v == needle)
}

/// Counters and duration sums the `observe` middleware and the Git admission
/// paths maintain in memory. Nothing here is written per request except a
/// single atomic increment, so the endpoint does not add a database write.
pub struct Recorder {
    http_requests: [AtomicU64; 10 * 6],
    /// Total request milliseconds per cell; the mean is this over the count.
    http_millis: [AtomicU64; 10 * 6],
    git_requests: [AtomicU64; 3 * 6],
    git_admissions: [AtomicU64; OUTCOMES.len() * REASONS.len()],
    push_admissions: [AtomicU64; 5],
    in_flight: AtomicU64,
    scrape_failures: AtomicU64,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            http_requests: std::array::from_fn(|_| AtomicU64::new(0)),
            http_millis: std::array::from_fn(|_| AtomicU64::new(0)),
            git_requests: std::array::from_fn(|_| AtomicU64::new(0)),
            git_admissions: std::array::from_fn(|_| AtomicU64::new(0)),
            push_admissions: std::array::from_fn(|_| AtomicU64::new(0)),
            in_flight: AtomicU64::new(0),
            scrape_failures: AtomicU64::new(0),
        }
    }
}

/// The array index of one method/status-class cell, from the already-resolved
/// label values.
fn cell(method: &str, status: &str) -> Option<usize> {
    let m = index_of(&METHODS, &method)?;
    let s = index_of(&STATUS_CLASSES, &status)?;
    Some(m * STATUS_CLASSES.len() + s)
}

impl Recorder {
    /// Counts one finished HTTP request and its duration.
    pub fn request(&self, method: &str, status: u16, elapsed: std::time::Duration) {
        let method = method_label(method);
        let class = status_class(status);
        let Some(i) = index_of(&METHODS, &method)
            .and_then(|m| index_of(&STATUS_CLASSES, &class).map(|s| m * 6 + s))
        else {
            return;
        };
        self.http_requests[i].fetch_add(1, Ordering::Relaxed);
        // Whole milliseconds, the same resolution the request log reports.
        let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.http_millis[i].fetch_add(millis, Ordering::Relaxed);
    }

    /// Counts one finished Git Smart HTTP request for `service`.
    pub fn git_request(&self, service: &'static str, status: u16) {
        let class = status_class(status);
        let Some(i) = index_of(&GIT_SERVICES, &service)
            .and_then(|s| index_of(&STATUS_CLASSES, &class).map(|c| s * 6 + c))
        else {
            return;
        };
        self.git_requests[i].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one admission decision: `succeeded` for an admitted request,
    /// `refused` with the reason category for a denied one.
    pub fn admission(&self, admitted: bool, reason: &str) {
        let (outcome, reason) = if admitted {
            ("succeeded", "none")
        } else {
            ("refused", reason)
        };
        let Some(i) = index_of(&OUTCOMES, &outcome)
            .and_then(|o| index_of(&REASONS, &reason).map(|r| o * REASONS.len() + r))
        else {
            return;
        };
        self.git_admissions[i].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one ref-update admission asked for by the push hooks.
    pub fn push_admission(&self, outcome: &'static str) {
        let Some(i) = index_of(&OUTCOMES, &outcome) else {
            return;
        };
        self.push_admissions[i].fetch_add(1, Ordering::Relaxed);
    }

    /// A request has started answering: counted while it runs, so a scrape
    /// reports at least itself.
    pub fn request_started(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// A request's response is ready. Saturating, so a decrement without a
    /// matching start can never wrap the gauge to `u64::MAX`.
    pub fn request_finished(&self) {
        let mut current = self.in_flight.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_sub(1) else {
                debug_assert!(false, "a request finished without starting");
                return;
            };
            match self.in_flight.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn scrape_failed(&self) {
        self.scrape_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// The recorder a request should count towards, attached by `client_ip_middleware`.
/// The request-logging middleware runs first (outermost), before it can read
/// an extractor's state, so the state reaches it as a request extension.
#[derive(Clone)]
pub struct ObservedRecorder(pub Option<Arc<Recorder>>);

/// A semaphore's permits in use: its configured size minus the ones still
/// available. Read at scrape time, never maintained per request, so it cannot
/// drift from the semaphore it describes and counts permits held anywhere
/// (a Git request, the merge worker, a direct housekeeping enqueue).
pub fn permits_in_use(slots: &tokio::sync::Semaphore, configured: usize) -> f64 {
    let available = slots.available_permits();
    configured.saturating_sub(available) as f64
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// Serves `GET /metrics`.
///
/// Off unless `metrics_enabled`. When on, a client that is not loopback is
/// refused with a bare `404` — status, empty body, no content type — unless the
/// operator set `metrics_allow_non_loopback`. It is not the HTML not-found page
/// an unknown path gets: it carries no exposition, so it discloses nothing.
/// The scrape is counted by the `observe` middleware like every other request,
/// so it appears in `githearth_http_requests_total` once it has finished, and
/// it counts itself in `githearth_http_requests_in_flight` while it runs.
///
/// A failed database read is **not** a `5xx`: the full published list is still
/// rendered, the unread gauges at `0`, and `githearth_metrics_scrape_failures_total`
/// moves, so the answer is a scrape Prometheus keeps. Nothing this endpoint can
/// answer is a `5xx`: a renderer defect is logged and counted at `200` rather
/// than handed to a scraper as a body it would discard.
pub async fn metrics(State(state): State<AppState>, client: ResolvedIp) -> Response {
    if !state.config.metrics_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(addr) = client.0.map(|c| c.addr()) else {
        // No peer address: only reachable with `Router::oneshot`, where there
        // is no client to check. Refuse rather than assume loopback.
        return StatusCode::NOT_FOUND.into_response();
    };
    // A client that is not loopback is refused. The address is the one the
    // server resolved (trusted-proxy aware), so a proxy on loopback forwarding
    // a scrape from elsewhere is refused unless the operator asked for it.
    if !is_loopback(addr) && !state.config.metrics_allow_non_loopback {
        return StatusCode::NOT_FOUND.into_response();
    }
    let registry = state.job_kinds.as_deref().unwrap_or(&[]);
    let mut text = String::with_capacity(8 * 1024);
    let mut failures = state.recorder.as_deref().map_or(0, |recorder| {
        recorder.scrape_failures.load(Ordering::Relaxed)
    });
    // A published line the format refuses is a build-time defect, not an
    // outage: `render` skips it, logs it and counts it, so the answer here is
    // always the `200` exposition a scraper keeps. No `5xx` arm exists, because
    // Prometheus discards the body of a failed scrape and would hide both the
    // rest of the list and the failure counter.
    render(&state, registry, &mut text, &mut failures).await;
    exposition(StatusCode::OK, text)
}

fn exposition(status: StatusCode, body: String) -> Response {
    let mut response = body.into_response();
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        CONTENT_TYPE.parse().expect("a static header value"),
    );
    response
}

/// Whether `addr` is a loopback address. An IPv4-mapped loopback
/// (`::ffff:127.0.0.1`) is loopback too; `client_ip` already normalises it,
/// but the check does not depend on that.
pub fn is_loopback(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Renders the full exposition. Every header line comes from [`PUBLISHED`], so
/// the output cannot contain a metric the document does not list.
///
/// `failures` is the scrape-failure counter this scrape publishes: a database
/// read that fails adds one and is visible in the same body. A published line
/// the format refuses is a build-time defect; it is skipped, logged and counted
/// the same way, so rendering always completes and the caller always answers
/// `200`.
pub async fn render(
    state: &AppState,
    registered_kinds: &[&'static str],
    out: &mut String,
    failures: &mut u64,
) {
    let db = DatabaseState::load(state).await;
    if db.any_failed() {
        *failures += 1;
        if let Some(recorder) = &state.recorder {
            recorder.scrape_failed();
        }
    }
    let recorder = state.recorder.as_deref();

    for def in PUBLISHED {
        writeln!(out, "# HELP {} {}", def.name, def.help).expect("writing to a String");
        writeln!(out, "# TYPE {} {}", def.name, def.kind.as_str()).expect("writing to a String");
        for labels in values(def, registered_kinds) {
            let value = measure(state, def, recorder, &db, &labels);
            let pairs: Vec<(&str, &str)> = def
                .labels
                .iter()
                .copied()
                .zip(labels.iter().map(String::as_str))
                .collect();
            // The scrape-failure sample is this scrape's count, not a value read
            // by `measure`: the recorder it reads is still being written.
            let value = if def.name == "githearth_metrics_scrape_failures_total" {
                *failures as f64
            } else {
                value
            };
            if !write_sample(out, def.name, &pairs, &number(value)) {
                // Every name, key and value is a compile-time constant tested
                // by `every_published_metric_is_named_and_bounded`, so this
                // cannot be reached from data or configuration. Do not fail the
                // scrape for it: skip the line, log the defect, and move the
                // failure counter (whose sample is written last, so this one is
                // in the same body).
                tracing::error!(
                    metric = def.name,
                    "a published metric was refused by the exposition format; its sample is \
                     omitted"
                );
                *failures += 1;
                if let Some(recorder) = recorder {
                    recorder.scrape_failed();
                }
            }
        }
    }
}

/// The label-value combinations one metric emits, in publication order. The
/// per-key sets come from [`declared_values`], so the renderer and the tests
/// cannot disagree.
fn values(def: &MetricDef, registered_kinds: &[&'static str]) -> Vec<Vec<String>> {
    if def.labels.is_empty() {
        return vec![Vec::new()];
    }
    let mut combos: Vec<Vec<String>> = vec![Vec::new()];
    for label in def.labels {
        let next: Vec<String> = match *label {
            "kind" => {
                // The binary's registry, never the rows: a kind that is not
                // registered is counted under `other`. `declared_values` fixes
                // the list; this substitutes the running binary's kinds.
                let mut kinds: Vec<String> =
                    registered_kinds.iter().map(|k| (*k).to_owned()).collect();
                kinds.push("other".to_owned());
                kinds
            }
            _ => declared_values(def.name, label)
                .unwrap_or_else(|| panic!("{} has no declared values for {label}", def.name))
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        let mut expanded = Vec::with_capacity(combos.len() * next.len());
        for combo in &combos {
            for value in &next {
                let mut combo = combo.clone();
                combo.push(value.clone());
                expanded.push(combo);
            }
        }
        combos = expanded;
    }
    combos
}

/// One sample's value. Every arm reads a measurement the server keeps; nothing
/// here is derived from an identifier.
fn measure(
    state: &AppState,
    def: &MetricDef,
    recorder: Option<&Recorder>,
    db: &DatabaseState,
    labels: &[String],
) -> f64 {
    match def.name {
        "githearth_git_in_flight"
        | "githearth_auth_slots_in_use"
        | "githearth_housekeeping_runs_in_flight" => permit_measure(state, def),
        _ => recorder_measure(def, recorder, db, labels),
    }
}

/// The measurements that need no `AppState`: the recorder's counters and the
/// database aggregates. Split from [`measure`] so a unit test can drive the
/// published list without building a server.
fn recorder_measure(
    def: &MetricDef,
    recorder: Option<&Recorder>,
    db: &DatabaseState,
    labels: &[String],
) -> f64 {
    let Some(recorder) = recorder else {
        return static_measure(def, db, labels);
    };
    match def.name {
        "githearth_http_requests_total" => cell(&labels[0], &labels[1]).map_or(0.0, |i| {
            recorder.http_requests[i].load(Ordering::Relaxed) as f64
        }),
        "githearth_http_request_duration_mean_seconds" => {
            let Some(i) = cell(&labels[0], &labels[1]) else {
                return 0.0;
            };
            let count = recorder.http_requests[i].load(Ordering::Relaxed);
            if count == 0 {
                return 0.0;
            }
            recorder.http_millis[i].load(Ordering::Relaxed) as f64 / 1000.0 / count as f64
        }
        "githearth_http_requests_in_flight" => recorder.in_flight.load(Ordering::Relaxed) as f64,
        "githearth_git_requests_total" => {
            let i = index_of(&GIT_SERVICES, &labels[0].as_str())
                .and_then(|s| index_of(&STATUS_CLASSES, &labels[1].as_str()).map(|c| s * 6 + c));
            i.map_or(0.0, |i| {
                recorder.git_requests[i].load(Ordering::Relaxed) as f64
            })
        }
        "githearth_git_admissions_total" => {
            let i = index_of(&OUTCOMES, &labels[0].as_str()).and_then(|o| {
                index_of(&REASONS, &labels[1].as_str()).map(|r| o * REASONS.len() + r)
            });
            i.map_or(0.0, |i| {
                recorder.git_admissions[i].load(Ordering::Relaxed) as f64
            })
        }
        "githearth_git_push_admissions_total" => index_of(&OUTCOMES, &labels[0].as_str())
            .map_or(0.0, |i| {
                recorder.push_admissions[i].load(Ordering::Relaxed) as f64
            }),
        "githearth_git_in_flight"
        | "githearth_auth_slots_in_use"
        | "githearth_housekeeping_runs_in_flight" => 0.0,
        "githearth_metrics_scrape_failures_total" => {
            recorder.scrape_failures.load(Ordering::Relaxed) as f64
        }
        _ => static_measure(def, db, labels),
    }
}

/// A slot gauge, read from the semaphore it names.
fn permit_measure(state: &AppState, def: &MetricDef) -> f64 {
    match def.name {
        "githearth_git_in_flight" => permits_in_use(&state.git_slots, state.config.git_concurrency),
        "githearth_auth_slots_in_use" => {
            permits_in_use(&state.auth_slots, state.config.auth_concurrency)
        }
        _ => permits_in_use(
            &state.housekeeping_slots,
            state.config.housekeeping_concurrency,
        ),
    }
}

/// The database-backed and configuration-backed gauges, which need no recorder.
fn static_measure(def: &MetricDef, db: &DatabaseState, labels: &[String]) -> f64 {
    match def.name {
        "githearth_jobs" => match labels[0].as_str() {
            "pending" => db.jobs_pending,
            "running" => db.jobs_running,
            "done" => db.jobs_done,
            "dead" => db.jobs_dead,
            _ => 0.0,
        },
        "githearth_jobs_by_kind" => {
            let running = labels[1] == "running";
            let counts = if running {
                &db.jobs_running_by_kind
            } else {
                &db.jobs_pending_by_kind
            };
            if labels[0] == "other" {
                counts.other
            } else {
                counts.named.get(&labels[0]).copied().unwrap_or(0.0)
            }
        }
        "githearth_housekeeping_repositories" => match labels[0].as_str() {
            "completed" => db.housekeeping_completed,
            "deferred" => db.housekeeping_deferred,
            "failed" => db.housekeeping_failed,
            "never" => db.housekeeping_never,
            _ => db.housekeeping_unknown,
        },
        "githearth_outbox_events" => match labels[0].as_str() {
            "delivered" => db.outbox_delivered,
            _ => db.outbox_undelivered,
        },
        _ => 0.0,
    }
}

#[derive(Default)]
struct KindCounts {
    named: std::collections::HashMap<String, f64>,
    other: f64,
}

/// The database-backed gauges, each with whether it was read at all this
/// scrape. A gauge whose query failed is *unknown*, not zero: the sample is
/// still written at `0` (so the published series does not appear and disappear)
/// and [`DatabaseState::any_failed`] raises
/// `githearth_metrics_scrape_failures_total`, so the scrape that could not read
/// its source is visible in the same body rather than being discarded with a
/// `5xx`.
#[derive(Default)]
struct DatabaseState {
    jobs_pending: f64,
    jobs_running: f64,
    jobs_done: f64,
    jobs_dead: f64,
    jobs_pending_by_kind: KindCounts,
    jobs_running_by_kind: KindCounts,
    housekeeping_completed: f64,
    housekeeping_deferred: f64,
    housekeeping_failed: f64,
    housekeeping_never: f64,
    housekeeping_unknown: f64,
    outbox_undelivered: f64,
    outbox_delivered: f64,
    /// Whether each database source was read: `jobs`, `repository_housekeeping`
    /// and `outbox_events`. `false` means the gauge above is the placeholder
    /// `0`, not a measurement.
    jobs_read: bool,
    housekeeping_read: bool,
    outbox_read: bool,
}

impl DatabaseState {
    /// Reads the three aggregates. A failed query leaves its gauges at `0` and
    /// records the failure; it never fails the scrape, because a `5xx` would
    /// make Prometheus discard the body that carries the failure counter.
    async fn load(state: &AppState) -> Self {
        let mut db = Self::default();
        let registered = state.job_kinds.as_deref().unwrap_or(&[]);
        if let Ok(rows) =
            sqlx::query("SELECT state, kind, count(*) AS n FROM jobs GROUP BY state, kind")
                .fetch_all(&state.pool)
                .await
        {
            for row in &rows {
                // A row whose columns a schema change broke is skipped, not
                // counted as a zero and not allowed to poison the whole
                // collection.
                let Ok(count) = row.try_get::<i64, _>("n") else {
                    continue;
                };
                let count = count as f64;
                let Ok(kind) = row.try_get::<String, _>("kind") else {
                    continue;
                };
                let Ok(state_name) = row.try_get::<String, _>("state") else {
                    continue;
                };
                match state_name.as_str() {
                    "pending" => {
                        db.jobs_pending += count;
                        add_kind(&mut db.jobs_pending_by_kind, &kind, count, registered);
                    }
                    "running" => {
                        db.jobs_running += count;
                        add_kind(&mut db.jobs_running_by_kind, &kind, count, registered);
                    }
                    "done" => db.jobs_done += count,
                    "dead" => db.jobs_dead += count,
                    _ => {}
                }
            }
            db.jobs_read = true;
        }

        // One row per repository that has a housekeeping record. A repository
        // with no row has never been scheduled and is not counted, so the total
        // is the number of repositories the scheduler has reached.
        if let Ok(rows) = sqlx::query(
            "SELECT last_outcome, count(*) AS n FROM repository_housekeeping GROUP BY last_outcome",
        )
        .fetch_all(&state.pool)
        .await
        {
            for row in &rows {
                let count = row.try_get::<i64, _>("n").unwrap_or(0) as f64;
                match row.try_get::<String, _>("last_outcome").as_deref() {
                    Ok("completed") => db.housekeeping_completed += count,
                    Ok("deferred") => db.housekeeping_deferred += count,
                    Ok("failed") => db.housekeeping_failed += count,
                    Ok("never") => db.housekeeping_never += count,
                    _ => db.housekeeping_unknown += count,
                }
            }
            db.housekeeping_read = true;
        }

        if let Ok(outbox) = sqlx::query(
            "SELECT count(*) FILTER (WHERE delivered_at IS NULL) AS undelivered, \
             count(*) FILTER (WHERE delivered_at IS NOT NULL) AS delivered FROM outbox_events",
        )
        .fetch_one(&state.pool)
        .await
        {
            db.outbox_undelivered = outbox.try_get::<i64, _>("undelivered").unwrap_or(0) as f64;
            db.outbox_delivered = outbox.try_get::<i64, _>("delivered").unwrap_or(0) as f64;
            db.outbox_read = true;
        }
        db
    }

    /// Whether any database source could not be read this scrape.
    fn any_failed(&self) -> bool {
        !self.jobs_read || !self.housekeeping_read || !self.outbox_read
    }
}

fn add_kind(counts: &mut KindCounts, kind: &str, count: f64, registered: &[&'static str]) {
    if registered.contains(&kind) {
        *counts.named.entry(kind.to_owned()).or_default() += count;
    } else {
        counts.other += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_value_with_a_control_character_is_refused() {
        // The format has no escape for a carriage return, a tab or a NUL; a raw
        // one splits or truncates the line for a scraper.
        for bad in ["a\rb", "a\tb", "a\0b", "\u{7}b"] {
            let mut out = String::new();
            assert!(
                !write_sample(&mut out, "good_name", &[("k", bad)], "1"),
                "{bad:?} was written"
            );
            assert!(out.is_empty(), "a refused line left {out:?}");
        }
        // A newline is the one control character the format does escape.
        let mut out = String::new();
        assert!(write_sample(&mut out, "good_name", &[("k", "a\nb")], "1"));
        assert_eq!(out, "good_name{k=\"a\\nb\"} 1\n");
    }

    #[test]
    fn the_in_flight_gauge_counts_from_the_start_and_never_goes_negative() {
        let recorder = Recorder::default();
        recorder.request_started();
        recorder.request_started();
        assert_eq!(recorder.in_flight.load(Ordering::Relaxed), 2);
        recorder.request_finished();
        assert_eq!(recorder.in_flight.load(Ordering::Relaxed), 1);
        recorder.request_finished();
        assert_eq!(recorder.in_flight.load(Ordering::Relaxed), 0);
        // The decrement is a checked subtraction, so a future path that
        // finishes twice reports 0 rather than `u64::MAX`; a debug build also
        // trips the assertion inside `request_finished`.
    }

    #[test]
    fn label_values_are_published_per_metric_not_as_a_union() {
        let admissions = published_values("githearth_git_admissions_total", "outcome");
        assert!(admissions.contains(&"refused".to_owned()));
        // Published on the housekeeping gauge, never on an admission counter.
        assert!(!admissions.contains(&"completed".to_owned()));
        let housekeeping = published_values("githearth_housekeeping_repositories", "outcome");
        assert!(!housekeeping.contains(&"refused".to_owned()));
        assert!(housekeeping.contains(&"completed".to_owned()));
        // A value on another metric's `state` is refused on this one.
        assert_eq!(
            published_values("githearth_outbox_events", "state"),
            vec!["undelivered".to_owned(), "delivered".to_owned()]
        );
        // A metric that does not carry the label has no values for it.
        assert!(published_values("githearth_jobs", "reason").is_empty());
        // The union is still available for the document's vocabulary.
        let anywhere = published_values_anywhere("outcome");
        assert!(anywhere.contains(&"refused".to_owned()));
        assert!(anywhere.contains(&"completed".to_owned()));
    }

    #[test]
    fn every_published_metric_is_named_and_bounded() {
        let mut names = std::collections::HashSet::new();
        for def in PUBLISHED {
            assert!(metric_name(def.name).is_some(), "{}", def.name);
            assert!(names.insert(def.name), "{} is published twice", def.name);
            assert!(!def.help.is_empty(), "{} has no help", def.name);
            for label in def.labels {
                assert!(metric_name(label).is_some(), "{} label {label}", def.name);
            }
            // Every label set is a product of small fixed sets, so one metric
            // cannot grow with the installation.
            let combos = label_values(def);
            assert!(
                combos <= MAX_SERIES_PER_METRIC,
                "{} may have {combos} series, the cap is {MAX_SERIES_PER_METRIC}",
                def.name
            );
        }
        assert!(definition("not_a_published_metric").is_none());
    }

    /// Renders the recorder-backed samples the way `render` does, without a
    /// database. The published list, the label sets and the measures are the
    /// production ones; only the database gauges are left at their defaults.
    fn render_recorder_only(recorder: &Recorder, kinds: &[&'static str]) -> String {
        let db = DatabaseState::default();
        let mut out = String::new();
        for def in PUBLISHED {
            writeln!(out, "# HELP {} {}", def.name, def.help).unwrap();
            writeln!(out, "# TYPE {} {}", def.name, def.kind.as_str()).unwrap();
            for labels in values(def, kinds) {
                let pairs: Vec<(&str, &str)> = def
                    .labels
                    .iter()
                    .copied()
                    .zip(labels.iter().map(String::as_str))
                    .collect();
                let value = recorder_measure(def, Some(recorder), &db, &labels);
                assert!(write_sample(&mut out, def.name, &pairs, &number(value)));
            }
        }
        out
    }

    #[test]
    fn the_recorder_measurements_reach_the_published_samples() {
        let recorder = Recorder::default();
        recorder.request("GET", 200, std::time::Duration::from_millis(3));
        recorder.request("POST", 401, std::time::Duration::from_millis(1));
        // A method outside the list must land in OTHER, never a new series.
        recorder.request("BREW", 999, std::time::Duration::from_millis(1));
        recorder.git_request("receive_pack", 200);
        recorder.admission(false, "insufficient_grant");
        recorder.admission(true, "none");
        recorder.push_admission("refused");
        let out = render_recorder_only(&recorder, &["repository.housekeeping"]);
        assert!(out.contains("githearth_http_requests_total{method=\"GET\",status=\"2xx\"} 1"));
        assert!(out.contains("githearth_http_requests_total{method=\"POST\",status=\"4xx\"} 1"));
        assert!(out.contains("githearth_http_requests_total{method=\"OTHER\",status=\"OTHER\"} 1"));
        assert!(
            !out.contains("BREW"),
            "an unpublished method leaked into the output"
        );
        assert!(
            out.contains("githearth_git_requests_total{service=\"receive_pack\",status=\"2xx\"} 1")
        );
        assert!(out.contains(
            "githearth_git_admissions_total{outcome=\"refused\",reason=\"insufficient_grant\"} 1"
        ));
        assert!(
            out.contains("githearth_git_admissions_total{outcome=\"succeeded\",reason=\"none\"} 1")
        );
        assert!(out.contains("githearth_git_push_admissions_total{outcome=\"refused\"} 1"));
        // An unrecorded cell is published with 0, not omitted.
        assert!(out.contains("githearth_http_requests_total{method=\"DELETE\",status=\"2xx\"} 0"));
        assert_eq!(
            out.lines().filter(|l| l.starts_with("# TYPE ")).count(),
            PUBLISHED.len()
        );
    }

    /// The registry `serve` builds and the published list must name the same
    /// kinds: a new handler registered without publishing its kind would hide
    /// its jobs under `other`, and a published kind with no handler would
    /// publish a series nothing can fill.
    #[test]
    fn serve_registers_only_published_kinds() {
        let mut registry = crate::jobs::Registry::new();
        crate::notifications::register(&mut registry);
        crate::codeowners::register(&mut registry);
        crate::housekeeping::register(
            &mut registry,
            &crate::config::Config {
                database_url: "postgres://localhost/unused".into(),
                ..crate::config::Config::default()
            },
        );
        let mut registered = registry.kinds();
        registered.sort_unstable();
        let mut published = JOB_KINDS.to_vec();
        published.sort_unstable();
        assert_eq!(registered, published);
    }

    #[test]
    fn a_job_kind_outside_the_registry_is_other_and_never_becomes_a_series() {
        assert_eq!(
            job_kind_label("repository.housekeeping", &["repository.housekeeping"]),
            "repository.housekeeping"
        );
        assert_eq!(
            job_kind_label("tenant-abc-sync", &["repository.housekeeping"]),
            "other"
        );
        let mut db = DatabaseState::default();
        add_kind(
            &mut db.jobs_pending_by_kind,
            "repository.housekeeping",
            2.0,
            &["repository.housekeeping"],
        );
        add_kind(
            &mut db.jobs_pending_by_kind,
            "tenant-abc-sync",
            3.0,
            &["repository.housekeeping"],
        );
        let def = definition("githearth_jobs_by_kind").unwrap();
        let named = static_measure(
            def,
            &db,
            &["repository.housekeeping".into(), "pending".into()],
        );
        let other = static_measure(def, &db, &["other".into(), "pending".into()]);
        assert_eq!((named, other), (2.0, 3.0));
    }

    #[test]
    fn escaping_covers_backslash_quote_and_newline() {
        assert_eq!(escape_label("plain"), "plain");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(escape_label("two\nlines"), "two\\nlines");
        assert_eq!(escape_label("tab\tis literal"), "tab\tis literal");
    }

    #[test]
    fn an_unrepresentable_name_is_refused_rather_than_escaped() {
        assert_eq!(metric_name("githearth_ok_1"), Some("githearth_ok_1"));
        assert_eq!(metric_name("_leading"), Some("_leading"));
        assert_eq!(metric_name("1starts_with_a_digit"), None);
        assert_eq!(metric_name("has-dash"), None);
        assert_eq!(metric_name("has space"), None);
        assert_eq!(metric_name("has\nnewline"), None);
        assert_eq!(metric_name(""), None);
        let mut out = String::new();
        assert!(!write_sample(&mut out, "bad-name", &[], "1"));
        assert!(out.is_empty(), "a refused sample wrote {out:?}");
        assert!(!write_sample(
            &mut out,
            "good_name",
            &[("bad-key", "v")],
            "1"
        ));
        assert!(write_sample(&mut out, "good_name", &[("k", "a\"b")], "1"));
        assert_eq!(out, "good_name{k=\"a\\\"b\"} 1\n");
    }

    #[test]
    fn values_are_formatted_as_floats_or_integers() {
        assert_eq!(number(3.0), "3");
        assert_eq!(number(0.25), "0.25");
        assert_eq!(number(f64::NAN), "0");
        assert_eq!(number(f64::INFINITY), "0");
    }

    #[test]
    fn methods_and_status_classes_outside_the_list_collapse_to_other() {
        assert_eq!(method_label("GET"), "GET");
        assert_eq!(method_label("get"), "OTHER");
        assert_eq!(method_label("BREW"), "OTHER");
        assert_eq!(status_class(204), "2xx");
        assert_eq!(status_class(418), "4xx");
        assert_eq!(status_class(503), "5xx");
        assert_eq!(status_class(99), "OTHER");
        assert_eq!(status_class(600), "OTHER");
    }

    #[test]
    fn admission_reasons_never_echo_their_message() {
        assert_eq!(
            admission_reason("permission denied: ref.push"),
            "insufficient_grant"
        );
        assert_eq!(
            admission_reason("protected branch: ref.delete"),
            "protected_ref"
        );
        assert_eq!(
            admission_reason("fatal: /srv/githearth/repositories/ab/cd.git is corrupt"),
            "other"
        );
        assert_eq!(
            admission_reason("invalid or expired credentials"),
            "invalid_credentials"
        );
    }

    #[test]
    fn loopback_covers_v4_v6_and_mapped_addresses() {
        assert!(is_loopback("127.0.0.1".parse().unwrap()));
        assert!(is_loopback("::1".parse().unwrap()));
        assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_loopback("10.0.0.1".parse().unwrap()));
        assert!(!is_loopback("2001:db8::1".parse().unwrap()));
    }
}
