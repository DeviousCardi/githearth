//! The metrics endpoint (M4.1 U14).
//!
//! The published list is `githearth_server::metrics::PUBLISHED`; every
//! assertion below compares the served text against it, so a metric or a label
//! key that is emitted without being published fails the test rather than being
//! discovered by an operator.
//!
//! A non-loopback client is simulated the way the server sees one: the peer is
//! `127.0.0.1`, configured as a trusted proxy, and `X-Forwarded-For` names the
//! client. That is the resolution the login throttle and the request log use.
//! The client addresses are taken from the RFC 5737 / RFC 3849 documentation
//! ranges so the "no address in the output" assertion cannot pass by accident.

use githearth_server::{AppState, MIGRATOR, config::Config, connect, metrics, router};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};
use uuid::Uuid;

const DATABASE: &str = "githearth_b5_metrics";

// ---------------------------------------------------------------------------
// Database and state
// ---------------------------------------------------------------------------

fn base_url() -> String {
    std::env::var("GITHEARTH_TEST_DATABASE_URL")
        .expect("set GITHEARTH_TEST_DATABASE_URL (see docs/foundation.md)")
}

async fn scratch_url() -> String {
    let url = base_url();
    let (origin, _) = url.rsplit_once('/').expect("a database URL with a name");
    let maintenance = PgPool::connect(&format!("{origin}/postgres"))
        .await
        .expect("connect to `postgres` to create the scratch database");
    // The tests in this binary run in parallel and all call this, so another
    // one may create the database between the check and the create: a
    // duplicate here is success, not a failure.
    if let Err(error) = sqlx::query(&format!("CREATE DATABASE {DATABASE}"))
        .execute(&maintenance)
        .await
    {
        let duplicate = error.as_database_error().is_some_and(|e| {
            e.code().as_deref() == Some("42P04") || e.code().as_deref() == Some("23505")
        });
        assert!(duplicate, "create the scratch database: {error}");
    }
    maintenance.close().await;
    format!("{origin}/{DATABASE}")
}

async fn scratch_pool() -> PgPool {
    let config = Config {
        database_url: scratch_url().await,
        database_max_connections: 4,
        storage_root: temp_storage(),
        ..Config::default()
    };
    let pool = connect(&config).await.expect("connect");
    MIGRATOR.run(&pool).await.expect("migrate");
    pool
}

/// An `AppState` on the scratch pool. The configuration's `database_url` is the
/// scratch database's, so `AppState::new`'s validation sees the URL the pool
/// already uses.
async fn state_with(pool: &PgPool, tweak: impl FnOnce(&mut Config)) -> (AppState, PathBuf) {
    let root = temp_storage();
    let mut config = Config {
        database_url: scratch_url().await,
        database_max_connections: 4,
        storage_root: root.clone(),
        ..Config::default()
    };
    tweak(&mut config);
    let state = AppState::new(pool.clone(), config).await.expect("AppState");
    (state, root)
}

/// A short storage root: the hook socket path must fit `sun_path`.
fn temp_storage() -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/gthm-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    std::fs::create_dir_all(&dir).expect("create the storage root");
    dir
}

fn cleanup(root: &std::path::Path) {
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn account(pool: &PgPool, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .fetch_one(pool)
    .await
    .expect("insert account")
}

async fn organization(pool: &PgPool, owner: Uuid, slug: &str, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(name)
        .execute(pool)
        .await
        .expect("insert organization");
    sqlx::query(
        "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await
    .expect("insert membership");
    sqlx::query(
        "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, granted_by) \
         VALUES ($1, 'owner', 'account', $2, 'organization', $3, $2)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(id)
    .execute(pool)
    .await
    .expect("insert grant");
    id
}

async fn repository(pool: &PgPool, org: Uuid, name: &str, creator: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, \
         visibility) VALUES ($1, $2, $3, $4, $5, 'active', 'private')",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(creator)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("insert repository");
    id
}

// ---------------------------------------------------------------------------
// A real HTTP listener, so the peer address exists
// ---------------------------------------------------------------------------

async fn serve(state: AppState) -> (tokio::sync::oneshot::Sender<()>, u16) {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (shutdown_tx, port)
}

/// One raw HTTP/1.0 request: the peer address and the headers are exactly what
/// is written, and no client library rewrites either.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

async fn raw(port: u16, path: &str, headers: &[(&str, &str)]) -> Response {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");
    // `path` may carry the leading slash (`metrics::PATH` does); the request
    // line needs exactly one.
    let path = path.trim_start_matches('/');
    let mut request = format!("GET /{path} HTTP/1.0\r\nHost: 127.0.0.1\r\n");
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.expect("read");
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or(("", &text));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no status line in: {text}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Response {
        status,
        headers,
        body: body.to_owned(),
    }
}

async fn scrape(port: u16) -> Response {
    raw(port, metrics::PATH, &[]).await
}

// ---------------------------------------------------------------------------
// Parsing the text exposition
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Exposition {
    /// `name` -> every label-key set seen on it, sorted.
    metrics: BTreeMap<String, BTreeSet<Vec<String>>>,
    /// `name` -> its `# TYPE` line.
    types: BTreeMap<String, String>,
    /// `name` -> whether a `# HELP` line was seen.
    helps: BTreeSet<String>,
    /// Every label key -> every value seen for it.
    label_values: BTreeMap<String, BTreeSet<String>>,
}

impl Exposition {
    fn parse(text: &str) -> Self {
        let mut parsed = Self::default();
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                if let Some(rest) = line.strip_prefix("# HELP ") {
                    if let Some(name) = rest.split_whitespace().next() {
                        parsed.helps.insert(name.to_owned());
                    }
                } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                    let mut parts = rest.split_whitespace();
                    match (parts.next(), parts.next()) {
                        (Some(name), Some(kind)) => {
                            parsed.types.insert(name.to_owned(), kind.to_owned());
                        }
                        _ => panic!("malformed TYPE line: {line}"),
                    }
                }
                continue;
            }
            let (head, value) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("malformed sample line: {line}"));
            value
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("not a number: {line}"));
            let (name, label_text) = match head.split_once('{') {
                Some((name, rest)) => (
                    name,
                    rest.strip_suffix('}')
                        .unwrap_or_else(|| panic!("unclosed label set: {line}")),
                ),
                None => (head, ""),
            };
            let mut keys = Vec::new();
            for pair in split_label_pairs(label_text) {
                let (key, value) = pair
                    .split_once('=')
                    .unwrap_or_else(|| panic!("malformed label: {line}"));
                let value = value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .unwrap_or_else(|| panic!("unquoted label value: {line}"));
                parsed
                    .label_values
                    .entry(key.to_owned())
                    .or_default()
                    .insert(unescape(value));
                keys.push(key.to_owned());
            }
            keys.sort();
            parsed
                .metrics
                .entry(name.to_owned())
                .or_default()
                .insert(keys);
        }
        parsed
    }
}

/// Splits `a="1",b="x,y"` on commas outside quotes.
fn split_label_pairs(labels: &str) -> Vec<&str> {
    if labels.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let bytes = labels.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'"' if index == 0 || bytes[index - 1] != b'\\' => quoted = !quoted,
            b',' if !quoted => {
                out.push(&labels[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    out.push(&labels[start..]);
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn sample_value(text: &str, name: &str, fragment: &str) -> f64 {
    let mut total = 0.0;
    for line in text.lines() {
        if line.starts_with(name)
            && line.contains(fragment)
            && let Some((_, value)) = line.rsplit_once(' ')
        {
            total += value
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("not a number: {line}"));
        }
    }
    total
}

fn contains_ipv4(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .any(|candidate| {
            let parts: Vec<&str> = candidate.split('.').collect();
            parts.len() == 4
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.parse::<u16>().is_ok())
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// U14: under the default configuration the endpoint is **off**. A loopback
/// client gets a plain `404` with no body, whether it asks with nothing or with
/// the scrape's own `Accept`.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn off_by_default_refuses_even_a_loopback_client() {
    let pool = scratch_pool().await;
    let (state, root) = state_with(&pool, |config| {
        assert!(!config.metrics_enabled, "the default must be off");
        assert!(!config.metrics_allow_non_loopback, "and loopback-only");
    })
    .await;
    let (shutdown, port) = serve(state).await;

    let response = scrape(port).await;
    assert_eq!(response.status, 404);
    assert!(
        response.body.is_empty(),
        "a refused scrape carried a body: {:?}",
        response.body
    );
    assert_ne!(
        response.header("content-type"),
        Some(metrics::CONTENT_TYPE),
        "a refused scrape carried the exposition content type"
    );
    let response = raw(port, metrics::PATH, &[("accept", metrics::CONTENT_TYPE)]).await;
    assert_eq!(response.status, 404);

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// U14: with the endpoint on, a **non-loopback** client is refused. The peer is
/// a trusted proxy, so the forwarded header decides, exactly as it does for the
/// login throttle; changing only the header changes only the answer.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn refuses_a_non_loopback_client_with_the_endpoint_enabled() {
    let pool = scratch_pool().await;
    let (state, root) = state_with(&pool, |config| {
        config.metrics_enabled = true;
        config.trusted_proxies = vec!["127.0.0.1".to_owned()];
    })
    .await;
    let (shutdown, port) = serve(state).await;

    // Loopback: the peer itself, no header.
    let response = raw(port, metrics::PATH, &[]).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("content-type"), Some(metrics::CONTENT_TYPE));
    assert!(
        response.body.contains("# TYPE "),
        "no exposition in: {}",
        response.body
    );

    // Loopback header: still loopback.
    let response = raw(port, metrics::PATH, &[("x-forwarded-for", "127.0.0.1")]).await;
    assert_eq!(response.status, 200);
    let response = raw(port, metrics::PATH, &[("x-forwarded-for", "::1")]).await;
    assert_eq!(response.status, 200);

    // A non-loopback client through the trusted proxy: refused, and the
    // refusal looks exactly like the endpoint being off.
    for client in ["198.51.100.7", "10.0.0.5", "192.0.2.9", "2001:db8::1"] {
        let response = raw(port, metrics::PATH, &[("x-forwarded-for", client)]).await;
        assert_eq!(response.status, 404, "client {client} was not refused");
        assert!(
            response.body.is_empty(),
            "the refusal carried {:?}",
            response.body
        );
        assert_ne!(response.header("content-type"), Some(metrics::CONTENT_TYPE));
    }

    // A malformed forwarded header falls back to the peer (loopback), which is
    // the documented behaviour, not a way to skip the check.
    let response = raw(port, metrics::PATH, &[("x-forwarded-for", "not an ip")]).await;
    assert_eq!(response.status, 200);

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// The explicit override: with `metrics_allow_non_loopback` the same forwarded
/// client that was refused above is answered. It exists so a deliberate
/// decision is expressible, and so the refusal above is the flag's absence and
/// not a check that would refuse everyone.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn the_override_lets_a_non_loopback_client_scrape() {
    let pool = scratch_pool().await;
    let (state, root) = state_with(&pool, |config| {
        config.metrics_enabled = true;
        config.metrics_allow_non_loopback = true;
        config.trusted_proxies = vec!["127.0.0.1".to_owned()];
    })
    .await;
    let (shutdown, port) = serve(state).await;

    let response = raw(port, metrics::PATH, &[("x-forwarded-for", "198.51.100.7")]).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("content-type"), Some(metrics::CONTENT_TYPE));

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// U14: every metric and every label key emitted is in the published list, and
/// every published metric carries its `# HELP` and `# TYPE` line. A metric the
/// code emits without publishing fails here, which is the point of the case.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn every_emitted_metric_and_label_is_published() {
    let pool = scratch_pool().await;
    let (state, root) =
        state_with(&pool, |config: &mut Config| config.metrics_enabled = true).await;
    let (shutdown, port) = serve(state).await;

    let response = scrape(port).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some(metrics::CONTENT_TYPE));
    let parsed = Exposition::parse(&response.body);

    let published: BTreeSet<&str> = metrics::PUBLISHED.iter().map(|def| def.name).collect();
    let emitted: BTreeSet<&str> = parsed.metrics.keys().map(String::as_str).collect();
    let unpublished: Vec<&&str> = emitted.difference(&published).collect();
    assert!(
        unpublished.is_empty(),
        "these metrics are emitted but not published: {unpublished:?}"
    );
    let missing: Vec<&&str> = published.difference(&emitted).collect();
    assert!(
        missing.is_empty(),
        "these published metrics were not emitted: {missing:?}"
    );

    for def in metrics::PUBLISHED {
        let kind = parsed
            .types
            .get(def.name)
            .unwrap_or_else(|| panic!("no # TYPE for {}", def.name));
        let expected = match def.kind {
            metrics::Kind::Counter => "counter",
            metrics::Kind::Gauge => "gauge",
        };
        assert_eq!(kind, expected, "wrong # TYPE for {}", def.name);
        assert!(
            parsed.helps.contains(def.name),
            "no # HELP for {}",
            def.name
        );
        let mut expected_keys: Vec<String> =
            def.labels.iter().map(|label| (*label).to_owned()).collect();
        expected_keys.sort();
        for keys in parsed.metrics.get(def.name).expect("emitted") {
            assert_eq!(
                keys, &expected_keys,
                "{} emitted a label set the published list does not describe",
                def.name
            );
        }
    }

    // Every label value came from the published set **for its own metric**: a
    // value published on another metric (an admission `outcome` labelled
    // `completed`) is still refused. A free-form value would be a new series an
    // operator never agreed to.
    for line in response.body.lines().filter(|l| !l.starts_with('#')) {
        let (head, _) = line.rsplit_once(' ').expect("a value");
        let (name, labels) = head.split_once('{').map_or((head, ""), |(name, rest)| {
            (name, rest.trim_end_matches('}'))
        });
        for pair in split_label_pairs(labels) {
            let (key, value) = pair.split_once('=').expect("a label");
            let value = value.trim_matches('"');
            let allowed: BTreeSet<String> =
                metrics::published_values(name, key).into_iter().collect();
            assert!(
                !allowed.is_empty(),
                "{name} has no published values for {key}"
            );
            assert!(
                allowed.contains(value),
                "label {key}=\"{value}\" on {name} is not one of its published values"
            );
        }
    }

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// Renames a table, so the query that reads it fails until it is renamed back.
/// The names are fixed here, never interpolated from data.
async fn rename(pool: &PgPool, from: &str, to: &str) {
    sqlx::query(&format!("ALTER TABLE {from} RENAME TO {to}"))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("rename {from} to {to}: {error}"));
}

/// A `200` exposition of the **whole** published list, whatever the source did.
fn assert_whole_exposition(source: &str, response: &Response) {
    assert_eq!(
        response.status, 200,
        "the {source} scrape must still be one Prometheus keeps: {}",
        response.body
    );
    assert_eq!(
        response.header("content-type"),
        Some(metrics::CONTENT_TYPE),
        "{source}"
    );
    let parsed = Exposition::parse(&response.body);
    let published: BTreeSet<&str> = metrics::PUBLISHED.iter().map(|def| def.name).collect();
    let emitted: BTreeSet<&str> = parsed.metrics.keys().map(String::as_str).collect();
    assert_eq!(
        emitted, published,
        "the {source} scrape did not carry the whole published list"
    );
}

fn failures(response: &Response) -> f64 {
    sample_value(
        &response.body,
        "githearth_metrics_scrape_failures_total",
        "",
    )
}

/// U14, B1: a scrape whose database read fails is still a scrape an operator
/// sees. Each database source is renamed away in turn — the outage every
/// database statement is subject to — and the answer must be a `200`
/// exposition of the **whole** published list, with that source's gauges at `0`
/// and `githearth_metrics_scrape_failures_total` incremented. `jobs` is forced
/// explicitly: it is the largest gauge, and a failed read that left the counter
/// still would be indistinguishable from a genuine zero. A `5xx` would be
/// discarded by Prometheus and would hide both the failure and the gauges.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_failed_query_still_answers_the_full_exposition_with_the_failure_counted() {
    let pool = scratch_pool().await;
    let (state, root) =
        state_with(&pool, |config: &mut Config| config.metrics_enabled = true).await;
    let (shutdown, port) = serve(state).await;

    // Each source is renamed away, scraped twice, then put back and scraped
    // once more. Every rename is undone before any assertion, so a failure
    // cannot leave the schema short a table (a migration will not re-create a
    // dropped one: its version is recorded).
    rename(&pool, "jobs", "jobs_forced_outage").await;
    let jobs_failed = scrape(port).await;
    // The next scrape fails the same way and the counter keeps moving; it is a
    // counter, not a one-off flag.
    let jobs_failed_again = scrape(port).await;
    rename(&pool, "jobs_forced_outage", "jobs").await;
    let jobs_restored = scrape(port).await;

    rename(&pool, "outbox_events", "outbox_events_forced_outage").await;
    let outbox_failed = scrape(port).await;
    let outbox_failed_again = scrape(port).await;
    rename(&pool, "outbox_events_forced_outage", "outbox_events").await;

    let _ = shutdown.send(());
    cleanup(&root);
    // The suite leaves the scratch database as it found it.
    MIGRATOR.run(&pool).await.expect("re-apply the migrations");

    for (source, response) in [
        ("jobs", &jobs_failed),
        ("jobs again", &jobs_failed_again),
        ("jobs restored", &jobs_restored),
        ("outbox_events", &outbox_failed),
        ("outbox_events again", &outbox_failed_again),
    ] {
        assert_whole_exposition(source, response);
    }

    // The source that could not be read is not invented: its series are present
    // and 0, rather than disappearing.
    for response in [&jobs_failed, &jobs_failed_again] {
        assert_eq!(
            sample_value(&response.body, "githearth_jobs", "state=\"pending\""),
            0.0
        );
        assert_eq!(
            sample_value(&response.body, "githearth_jobs", "state=\"running\""),
            0.0
        );
        assert_eq!(
            sample_value(
                &response.body,
                "githearth_jobs_by_kind",
                "kind=\"other\",state=\"pending\""
            ),
            0.0
        );
    }
    for response in [&outbox_failed, &outbox_failed_again] {
        assert_eq!(
            sample_value(
                &response.body,
                "githearth_outbox_events",
                "state=\"delivered\""
            ),
            0.0
        );
        assert_eq!(
            sample_value(
                &response.body,
                "githearth_outbox_events",
                "state=\"undelivered\""
            ),
            0.0
        );
    }

    // The counter moves once for each failed scrape, for both sources, and does
    // not move for the healthy scrape in between. It is the signal that tells
    // an unreadable gauge from a genuine zero.
    assert_eq!(
        [
            failures(&jobs_failed),
            failures(&jobs_failed_again),
            failures(&jobs_restored),
            failures(&outbox_failed),
            failures(&outbox_failed_again),
        ],
        [1.0, 2.0, 2.0, 3.0, 4.0],
        "the failure counter did not move once per failed source"
    );

    pool.close().await;
}

/// M4: `githearth_http_requests_in_flight` counts requests that are being
/// answered, so a scrape during another request's handler sees at least one.
/// A gauge incremented after the response is written would always read 0 here,
/// because the scrape itself has not finished either.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_request_in_flight_is_visible_to_a_scrape_during_it() {
    let db = scratch_pool().await;
    let (state, _root) = state_with(&db, |config: &mut Config| config.metrics_enabled = true).await;
    // A route that holds its request open until the test lets go, so one
    // request is provably in flight while another is answered. It is merged
    // over the production router, so both requests go through the real
    // `observe` middleware and the real `/metrics` handler.
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    let app = std::sync::Arc::new(router(state).route(
        "/test-slow",
        axum::routing::get(move || {
            let held = held.clone();
            async move {
                held.notified().await;
                "held"
            }
        }),
    ));

    async fn call(app: &std::sync::Arc<axum::Router>, path: &str) -> (u16, String) {
        use tower::util::ServiceExt as _;
        let request = axum::http::Request::builder()
            .uri(path)
            .header("host", "127.0.0.1")
            .body(axum::body::Body::empty())
            .expect("a request");
        // `oneshot` has no peer, and the endpoint refuses without one, so the
        // address is injected the way `client_ip_middleware` does it.
        let mut request = request;
        client_addressed(&mut request);
        let response = app
            .as_ref()
            .clone()
            .oneshot(request)
            .await
            .expect("a response");
        let status = response.status().as_u16();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    let slow = {
        let app = app.clone();
        tokio::spawn(async move { call(&app, "/test-slow").await })
    };
    // Let the slow request reach its handler before scraping.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!slow.is_finished(), "the slow request finished early");

    let (status, body) = call(&app, metrics::PATH).await;
    assert_eq!(status, 200, "{body}");
    let in_flight = sample_value(&body, "githearth_http_requests_in_flight", "");
    assert!(
        in_flight >= 1.0,
        "in_flight was {in_flight} with two requests being answered\n{body}"
    );

    release.notify_waiters();
    let (status, _) = slow.await.expect("the slow request");
    assert_eq!(status, 200);

    db.close().await;
}
/// The `ClientIp` extension `client_ip_middleware` inserts, for a request made
/// with `Router::oneshot` (which has no peer address, and so is refused).
fn client_addressed(request: &mut axum::http::Request<axum::body::Body>) {
    let ip =
        githearth_server::client_ip::ClientIp::resolved("127.0.0.1".parse().expect("loopback"));
    request.extensions_mut().insert(ip);
}

/// M2: the refusal is a bare `404` — status, no body, no exposition content
/// type — and not the HTML not-found page. It carries nothing an operator's
/// scraper could mistake for samples.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn the_refusal_is_a_bare_404_and_not_the_not_found_page() {
    let pool = scratch_pool().await;
    let (state, root) = state_with(&pool, |config| {
        config.metrics_enabled = true;
        config.trusted_proxies = vec!["127.0.0.1".to_owned()];
    })
    .await;
    let (shutdown, port) = serve(state).await;

    let refused = raw(port, metrics::PATH, &[("x-forwarded-for", "198.51.100.7")]).await;
    assert_eq!(refused.status, 404);
    assert!(refused.body.is_empty(), "{:?}", refused.body);
    assert!(
        !refused.body.contains("<html"),
        "the refusal served the not-found page"
    );
    assert_ne!(refused.header("content-type"), Some(metrics::CONTENT_TYPE));

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// U14: after requests that name an organization, a repository, an account, an
/// email and an address, the scrape contains none of them. The names are
/// random per run, so a pass cannot come from a stale fixture.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn the_output_carries_no_organization_repository_account_email_or_address() {
    let pool = scratch_pool().await;
    let (state, root) = state_with(&pool, |config| {
        config.metrics_enabled = true;
        config.trusted_proxies = vec!["127.0.0.1".to_owned()];
    })
    .await;
    let (shutdown, port) = serve(state).await;

    let tag = Uuid::new_v4().simple().to_string()[..10].to_owned();
    let username = format!("user-{tag}");
    let email = format!("{username}@example-tests.invalid");
    let slug = format!("org-{tag}");
    let org_name = format!("Org Display {tag}");
    let repo_name = format!("repo-{tag}");
    let owner = account(&pool, &username).await;
    sqlx::query("UPDATE accounts SET email = $1 WHERE id = $2")
        .bind(&email)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("set the email");
    let org = organization(&pool, owner, &slug, &org_name).await;
    let repo = repository(&pool, org, &repo_name, owner).await;

    // Requests that name all of them: an anonymous Git probe, a browse path
    // and an API read. They are refused (no credential); the point is that the
    // names reach a handler and a log line, and the scrape still shows none.
    for path in [
        format!("{slug}/{repo_name}.git/info/refs?service=git-upload-pack"),
        format!("{slug}/{repo_name}/issues"),
        format!("api/v1/organizations/{slug}/repositories/{repo_name}"),
    ] {
        let _ = raw(port, &path, &[]).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

    let response = raw(port, metrics::PATH, &[("x-forwarded-for", "127.0.0.1")]).await;
    assert_eq!(response.status, 200);
    let text = &response.body;

    // Evidence the operations were counted at all: some GET answered.
    assert!(
        sample_value(text, "githearth_http_requests_total", "method=\"GET\"") > 0.0,
        "no GET was counted in the scrape:\n{text}"
    );

    let secrets = [
        username.as_str(),
        email.as_str(),
        slug.as_str(),
        org_name.as_str(),
        repo_name.as_str(),
        &org.to_string(),
        &repo.to_string(),
        &owner.to_string(),
    ];
    for secret in secrets {
        assert!(!text.contains(secret), "the scrape contains {secret:?}");
    }
    assert!(!contains_ipv4(text), "the scrape contains an address");
    assert!(
        !text.contains("127.0.0.1"),
        "the peer address is in the output"
    );
    assert!(
        !text.to_lowercase().contains("example-tests"),
        "the email domain leaked"
    );

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// M3: the document and the code cannot drift apart. `tests/metrics.rs`'s other
/// published-list test compares the served text with `metrics::PUBLISHED`, which
/// the renderer iterates — so a metric added to that table would pass it while
/// `docs/operations/metrics.md` stayed silent. This test reads the document
/// itself (`include_str!`, so it is the reviewed file, not a copy) and asserts
/// set equality with `PUBLISHED` for names, types and label keys, and each
/// label value against **its own metric's** published set.
#[test]
fn the_document_and_the_published_list_agree() {
    const DOC: &str = include_str!("../../../docs/operations/metrics.md");
    const HEADER: &str = "| Metric | Type | Labels | Meaning |";
    let rows = table_rows(DOC, HEADER);
    assert!(
        rows.len() >= 10,
        "the metric table was not found in metrics.md ({} rows)",
        rows.len()
    );

    // Names, types and label keys, in both directions.
    let documented: BTreeMap<String, (String, Vec<String>)> = rows
        .iter()
        .map(|cells| {
            let name = cells[0].trim_matches('`').to_owned();
            let kind = cells[1].trim().to_owned();
            let labels = if cells[2].trim() == "—" {
                Vec::new()
            } else {
                cells[2]
                    .split(',')
                    .map(|label| label.trim().trim_matches('`').to_owned())
                    .collect()
            };
            (name, (kind, labels))
        })
        .collect();
    let published: BTreeSet<&str> = metrics::PUBLISHED.iter().map(|def| def.name).collect();
    assert_eq!(
        documented
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        published,
        "the metric table and PUBLISHED name different metrics"
    );
    for def in metrics::PUBLISHED {
        let (kind, labels) = &documented[def.name];
        assert_eq!(
            kind,
            match def.kind {
                metrics::Kind::Counter => "counter",
                metrics::Kind::Gauge => "gauge",
            },
            "metrics.md gives {} the wrong type",
            def.name
        );
        assert_eq!(
            labels, def.labels,
            "metrics.md gives {} other labels",
            def.name
        );
    }

    // Label values, per metric: each metric's set for a key is exactly one
    // documented row — the one that names that metric, or the unqualified one
    // when no other metric has a different set for the key. A value published
    // on one metric is therefore not accepted on another.
    let value_rows: Vec<(String, Option<String>, BTreeSet<String>)> =
        table_rows(DOC, "| Label | Values |")
            .iter()
            .map(|cells| {
                // "`outcome` (housekeeping)" names the metric's own set; the
                // parenthesised word selects the metric that has one.
                let (key, qualifier) = match cells[0].split_once('(') {
                    Some((key, rest)) => (
                        key.trim().trim_matches('`').to_owned(),
                        Some(rest.trim_matches(')').trim().to_owned()),
                    ),
                    None => (cells[0].trim_matches('`').to_owned(), None),
                };
                let values = cells[1]
                    .split(',')
                    .map(|v| v.trim())
                    .filter(|v| !v.is_empty() && !v.starts_with('('))
                    .map(|v| v.trim_matches('`').to_owned())
                    .collect();
                (key, qualifier, values)
            })
            .collect();
    assert!(
        !value_rows.is_empty(),
        "the label-value table was not found"
    );

    // For each key: a row qualified with a word of a metric's name
    // ("`outcome` (admissions)") fixes that metric's set; the unqualified row,
    // when there is one, fixes every metric that no qualified row names. A
    // metric that matches no row at all fails, and so does a qualified row
    // that names no metric.
    let keys: BTreeSet<String> = metrics::PUBLISHED
        .iter()
        .flat_map(|def| def.labels.iter().map(|label| (*label).to_owned()))
        .collect();
    for key in keys {
        let owners: Vec<&metrics::MetricDef> = metrics::PUBLISHED
            .iter()
            .filter(|def| def.labels.contains(&key.as_str()))
            .collect();
        let rows: Vec<&(String, Option<String>, BTreeSet<String>)> =
            value_rows.iter().filter(|(k, _, _)| *k == key).collect();
        assert!(!rows.is_empty(), "metrics.md does not document label {key}");

        // Every qualified row names its metric outright, and that metric's set
        // is exactly the row's.
        for (_, qualifier, documented) in &rows {
            let Some(word) = qualifier else { continue };
            let named: Vec<&str> = owners
                .iter()
                .map(|def| def.name)
                .filter(|name| *name == word.trim_matches('`'))
                .collect();
            assert_eq!(
                named.len(),
                1,
                "metrics.md's `{key} ({word})` row names {named:?}, not one metric"
            );
            let code: BTreeSet<String> = metrics::published_values(named[0], &key)
                .into_iter()
                .collect();
            assert_eq!(
                &code, documented,
                "metrics.md's `{key} ({word})` row and {}'s renderer disagree",
                named[0]
            );
        }

        // Every metric carrying the key is described by the row that fits it,
        // and by that one only.
        for def in owners {
            let fitting: Vec<&BTreeSet<String>> = rows
                .iter()
                .filter(|(_, qualifier, _)| match qualifier {
                    Some(word) => def.name == word.trim_matches('`'),
                    None => !rows.iter().any(|(_, q, _)| {
                        q.as_deref()
                            .is_some_and(|w| def.name == w.trim_matches('`'))
                    }),
                })
                .map(|(_, _, values)| values)
                .collect();
            assert_eq!(
                fitting.len(),
                1,
                "metrics.md describes {key} on {} by {} rows",
                def.name,
                fitting.len()
            );
            let code: BTreeSet<String> = metrics::published_values(def.name, &key)
                .into_iter()
                .collect();
            assert_eq!(
                code, *fitting[0],
                "metrics.md's `{key}` row and {}'s renderer disagree",
                def.name
            );
        }

        // Nothing published for a key is missing from the document, and the
        // document names nothing no metric can emit.
        let documented: BTreeSet<String> = rows
            .iter()
            .flat_map(|(_, _, values)| values.iter().cloned())
            .collect();
        let anywhere: BTreeSet<String> = metrics::published_values_anywhere(&key)
            .into_iter()
            .collect();
        assert_eq!(
            anywhere, documented,
            "metrics.md and the renderer publish different values for {key:?}"
        );
    }
}

/// The body rows of the markdown table whose header line is `header`, each
/// already split into its cells (trimmed, leading and trailing `|` removed).
fn table_rows(doc: &str, header: &str) -> Vec<Vec<String>> {
    let mut lines = doc.lines();
    let Some(_) = lines.find(|line| line.trim() == header) else {
        return Vec::new();
    };
    // Skip the `| --- |` separator line.
    lines.next();
    lines
        .take_while(|line| line.trim_start().starts_with('|'))
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect()
        })
        .collect()
}

/// "Real measurements, not invented ones": a request that runs a counted
/// operation moves that operation's counter. Two finished requests must raise
/// the GET counter by at least two, and an anonymous Git admission must raise
/// the admission counter by at least one.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_measured_operation_moves_its_counter() {
    let pool = scratch_pool().await;
    let (state, root) =
        state_with(&pool, |config: &mut Config| config.metrics_enabled = true).await;
    let (shutdown, port) = serve(state).await;

    let before = scrape(port).await.body;
    let before_get = sample_value(&before, "githearth_http_requests_total", "method=\"GET\"");
    let before_refused = sample_value(
        &before,
        "githearth_git_admissions_total",
        "outcome=\"refused\"",
    );

    // An anonymous Git probe of a repository that does not exist: admission
    // refuses it (one `refused` admission), and the HTTP request finishes.
    let _ = raw(
        port,
        "no-such-org/no-such-repo.git/info/refs?service=git-upload-pack",
        &[],
    )
    .await;
    let _ = raw(port, "health/live", &[]).await;
    let _ = raw(port, "health/live", &[]).await;
    // The middleware counts a request after the handler returns; the scrape
    // itself is a further request, so give the counter a moment to settle.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let after = scrape(port).await.body;
    let after_get = sample_value(&after, "githearth_http_requests_total", "method=\"GET\"");
    let after_refused = sample_value(
        &after,
        "githearth_git_admissions_total",
        "outcome=\"refused\"",
    );

    assert!(
        after_get >= before_get + 3.0,
        "the GET counter did not move for three requests: {before_get} -> {after_get}\n{after}"
    );
    assert!(
        after_refused > before_refused,
        "the Git admission counter did not move for a refused probe: {before_refused} -> \
         {after_refused}\n{after}"
    );

    let _ = shutdown.send(());
    cleanup(&root);
    pool.close().await;
}

/// U14: a label value that needs escaping is escaped and the sample stays one
/// line. The published sets cannot contain such a value today, so this drives
/// the writer the exposition is built from: if a published label ever grew a
/// free-form value, the writer would still produce a parseable line.
#[test]
fn a_label_value_that_needs_escaping_is_escaped() {
    let mut out = String::new();
    assert!(metrics::write_sample(
        &mut out,
        "githearth_test_escape",
        &[("value", "a\"b\\c\nd")],
        "1",
    ));
    assert_eq!(out, "githearth_test_escape{value=\"a\\\"b\\\\c\\nd\"} 1\n");
    assert_eq!(
        out.lines().count(),
        1,
        "the newline in a value broke the line"
    );
    let parsed = Exposition::parse(&format!("# TYPE githearth_test_escape gauge\n{out}"));
    assert!(
        parsed
            .label_values
            .get("value")
            .is_some_and(|values| values.contains("a\"b\\c\nd")),
        "the value did not round-trip: {parsed:?}"
    );
    assert_eq!(metrics::escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    assert_eq!(metrics::escape_label("plain"), "plain");

    // A name or key the format cannot carry is refused, and refusal is atomic:
    // nothing half-written is left for the next sample to follow.
    let mut out = String::new();
    assert!(!metrics::write_sample(&mut out, "has dash", &[], "1"));
    assert!(!metrics::write_sample(
        &mut out,
        "ok_name",
        &[("bad-key", "v")],
        "1"
    ));
    assert!(out.is_empty(), "a refused line left {out:?}");
    assert!(metrics::write_sample(
        &mut out,
        "ok_name",
        &[("good_key", "v")],
        "1"
    ));
    assert_eq!(out, "ok_name{good_key=\"v\"} 1\n");
}
