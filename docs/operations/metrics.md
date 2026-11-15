# Metrics

`GET /metrics` publishes a fixed set of measurements in Prometheus text
exposition format, for an operator's own scrape. It is **off by default**, and
when it is on it answers **only a loopback client** unless the operator
explicitly allows more. An installation that never turns it on never exposes
it, and a client that is not loopback gets a bare `404` — a status, no body and
no content type — that carries no exposition and discloses nothing. It is not
the HTML not-found page an unknown path gets; the endpoint does not confirm
that it exists.

The acceptance case is U14: *the endpoint is off or loopback-only by default
and exposes only the published metric and label list, which contains no
organization, repository or account names.*

## Configuration

| Setting | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `metrics_enabled` | `GITHEARTH_METRICS_ENABLED` | `false` | Answer `GET /metrics`. Off means the handler refuses every scrape with `404`, loopback or not |
| `metrics_allow_non_loopback` | `GITHEARTH_METRICS_ALLOW_NON_LOOPBACK` | `false` | Let a client that is not loopback scrape too. Refused at load unless `metrics_enabled = true`, because on its own it would do nothing |

```toml
metrics_enabled = true
```

Restart `serve` after changing either setting: the endpoint reads the
configuration of the running process, not the file.

There is no separate listener. A second `bind` would mean a second port to
firewall, a second TLS decision and a second systemd unit, for one `GET` that
the main listener already serves. The loopback check is **the boundary** — with
one exception, `metrics_allow_non_loopback`, which an operator sets only after
deciding the listener is reachable by nobody they do not trust (see below). The
check runs per request, so the answer never depends on where the process was
started.

### How a non-loopback client is refused

The check uses the address the server has **already resolved** for the request
— the peer, or, when the peer is a configured `trusted_proxies` entry, the
rightmost untrusted `X-Forwarded-For` address. It is the same value the failed
sign-in throttle stores and the request log prints, so the two cannot disagree
about who a client is.

- no trusted proxy, peer `127.0.0.1` or `::1`: **200**;
- with `trusted_proxies = ["127.0.0.1"]` and `X-Forwarded-For: 198.51.100.7`
  from a loopback peer: **404**;
- an address that is neither loopback nor forwarded: **404**.

The refusal is a bare `404` with an empty body and no content type. It is not a
`401`, and it is not the 1.6 KB HTML page an unknown path gets: it carries no
exposition, no reason and no headers that would say the endpoint exists. A
malformed `X-Forwarded-For` falls back to the peer, which is the same
conservative behaviour every other client-address decision uses.

If Prometheus runs on another host, do not set `metrics_allow_non_loopback`:
forward the scrape through the reverse proxy that already terminates TLS, so
the scrape arrives from the proxy's loopback address. Set the flag only when
the listener is on a private interface that nothing else can reach, and set
`metrics_enabled = true` in the same file.

## The published metric list

Every metric this endpoint may emit is in this table, and the endpoint emits
only what is in it. Two tests guard that promise: one parses the served text
and fails on a metric, a label key or a label value that is not below; the
other reads **this file** (`include_str!` in
`crates/githearth-server/tests/metrics.rs`) and fails when this table and the
code's published list disagree in either direction. A metric added to the code
without a row here, or a row here the code cannot emit, fails the build.

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `githearth_http_requests_total` | counter | `method`, `status` | HTTP requests answered since start |
| `githearth_http_request_duration_mean_seconds` | gauge | `method`, `status` | Mean request duration in seconds, from whole-millisecond samples like the request log, so its resolution is `0.001` s |
| `githearth_http_requests_in_flight` | gauge | — | Requests being answered right now, including the scrape that reads this |
| `githearth_git_requests_total` | counter | `service`, `status` | Completed Git Smart HTTP requests |
| `githearth_git_admissions_total` | counter | `outcome`, `reason` | Git admission decisions |
| `githearth_git_push_admissions_total` | counter | `outcome` | Ref-update admissions the push hooks asked for |
| `githearth_jobs` | gauge | `state` | Jobs in the durable queue |
| `githearth_jobs_by_kind` | gauge | `kind`, `state` | Jobs pending or running, by registered kind |
| `githearth_housekeeping_repositories` | gauge | `outcome` | Repositories by their last housekeeping outcome |
| `githearth_outbox_events` | gauge | `state` | Committed outbox events, by delivery |
| `githearth_git_in_flight` | gauge | — | Git slots in use |
| `githearth_auth_slots_in_use` | gauge | — | Sign-in checks holding an authentication slot |
| `githearth_housekeeping_runs_in_flight` | gauge | — | Housekeeping runs holding a housekeeping slot |
| `githearth_metrics_scrape_failures_total` | counter | — | Scrapes that could not read a database source; the gauges it could not read are reported as `0` |

The per-kind and slot gauges are read from the process or from `count(*)` at
scrape time; nothing here is written per request.

### The published label values

Every label value is drawn from a fixed set. No value is derived from a row, so
the number of series a metric can have is fixed by this table, not by the size
of the installation. A value that is not listed for **its own metric** is
refused by the acceptance test: `outcome="completed"` is published on
`githearth_housekeeping_repositories` and refused on an admissions counter.

| Label | Values |
| --- | --- |
| `method` | `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`, `OPTIONS`, `TRACE`, `CONNECT`, `OTHER` |
| `status` | `1xx`, `2xx`, `3xx`, `4xx`, `5xx`, `OTHER` |
| `service` | `upload_pack`, `receive_pack`, `info_refs` |
| `outcome` (`githearth_git_admissions_total`) | `succeeded`, `refused`, `deferred`, `skipped`, `failed` |
| `outcome` (`githearth_git_push_admissions_total`) | `succeeded`, `refused`, `deferred`, `skipped`, `failed` |
| `outcome` (`githearth_housekeeping_repositories`) | `completed`, `deferred`, `failed`, `never`, `unknown` |
| `reason` | `none`, `invalid_credentials`, `not_found`, `insufficient_grant`, `not_clonable`, `read_only`, `protected_ref`, `ref_namespace`, `throttled`, `busy`, `conflict`, `bad_request`, `too_large`, `unavailable`, `internal`, `other` |
| `state` (`githearth_jobs`) | `pending`, `running`, `done`, `dead` |
| `state` (`githearth_jobs_by_kind`) | `pending`, `running` |
| `state` (`githearth_outbox_events`) | `undelivered`, `delivered` |
| `kind` | `codeowners.request_reviews`, `notification.outbox`, `notification.pull_request_merged`, `notification.pull_request_reviewed`, `notification.pull_request_review_requested`, `repository.housekeeping`, `other` |

`OTHER` on `method` and `status`, and `other` on `reason` and `kind`, are
deliberate buckets: a method token the list does not name lands in `OTHER`
instead of becoming a new series, a refusal whose message is not in the fixed
set lands in `reason="other"` instead of echoing the message (which can name a
path or an object id), and a job row whose kind this binary did not register
lands in `kind="other"` (so a row left by an older binary never becomes a
series).

### What is deliberately not here

- **No organization, repository or account.** No ID, no slug, no name, no
  username, no email, no client address, no token. Nothing in the output tells
  one tenant's traffic from another's.
- **No per-route HTTP label.** The request log names the matched route, but a
  route label plus a tenant's paths would let a scrape estimate another
  tenant's repository names. `method`/`status` is enough to alert on.
- **No Git journal delay, push size or per-repository bytes.** Those need a
  query per repository, or a new write per push, and a per-repository label
  would be exactly the unbounded cardinality this page exists to prevent.
  `housekeeping_repositories` counts outcomes without naming a repository.
- **No quantiles.** The endpoint keeps a count and a duration sum per
  method/status cell, both in memory, so it can offer a mean. A histogram
  needs a bucket set per cell and a decision about which buckets matter; that
  is a later change, and the mean is honest until then.
- **Housekeeping runs** are visible as `githearth_jobs_by_kind{kind="repository.housekeeping"}`
  (pending/running) and as `githearth_housekeeping_repositories{outcome}`. The
  bytes-before/after numbers a run records are per repository and are in
  `repository_housekeeping` ([housekeeping.md](housekeeping.md#results)).

The outbox delivery states are read from `outbox_events.delivered_at`. The
per-consumer fan-out relay that M4.1 6.3 adds will add its own states; this
list changes in the same change that adds them.

## Sample scrape

An enabled endpoint, scraped from loopback with no trusted proxy configured.
This is an **excerpt** of the body: it starts with `githearth_http_requests_total`,
and the excerpt below is the `in_flight` block further down:

```console
$ curl -s http://127.0.0.1:8080/metrics \
    | grep -A2 '^# HELP githearth_http_requests_in_flight'
# HELP githearth_http_requests_in_flight HTTP requests currently being answered, including the scrape that reads this.
# TYPE githearth_http_requests_in_flight gauge
githearth_http_requests_in_flight 1
```

`in_flight` reads `1`: the scrape counts itself while it runs.

The refusal needs a non-loopback *resolved* client. With no trusted proxy the
peer is the client, so a header alone changes nothing and the same `curl` from
loopback answers `200`. With `trusted_proxies = ["127.0.0.1"]` set, the peer is
the proxy and the forwarded address decides:

```console
# trusted_proxies = ["127.0.0.1"] in the running configuration
$ curl -s -H 'X-Forwarded-For: 198.51.100.7' -o /dev/null -w '%{http_code}\n' \
    http://127.0.0.1:8080/metrics
404
```

Without that `trusted_proxies` entry the same command prints `200`, because the
resolved client is the loopback peer.

## Format and escaping

The output is Prometheus text exposition, version `0.0.4`
(`Content-Type: text/plain; version=0.0.4; charset=utf-8`), written by hand in
`crates/githearth-server/src/metrics.rs`. No crate is added: the format is a
`# HELP` line, a `# TYPE` line and one line per sample, and a metrics crate
would still need the published list to be a fixed table, which is the part that
matters here.

- A metric name is checked against `[a-zA-Z_:][a-zA-Z0-9_:]*` and refused
  otherwise.
- A label value containing a control character other than a newline (a carriage
  return, a tab, a NUL) is refused: the format has no escape for it. A newline
  is escaped as `\n`, and backslash and double quote as `\\` and `\"`.
- A refused name, key or value writes nothing at all — the line is built in
  scratch — so a bad line can never follow the previous one onto the same
  input.
- Every metric emits a `0` for a cell that has not been used yet, so a
  dashboard sees the series from the first scrape.

## When a scrape cannot read the database

Four gauges are aggregates over the database: `githearth_jobs`,
`githearth_jobs_by_kind`, `githearth_housekeeping_repositories` and
`githearth_outbox_events`. They are unavailable exactly when the database is.

A scrape that cannot read one of them is still a scrape: it answers **`200`**
with the **whole** published list. The gauges whose source could not be read
are reported as `0`, and `githearth_metrics_scrape_failures_total` is
incremented **in that same body**, so the scrape that failed says so. Nothing
is invented: no gauge is given a value that was not measured, and no series is
added or omitted.

**A gauge that was unreadable is not distinguishable from a gauge that is
genuinely `0`.** The exposition format has no "unknown" sample, and writing one
any other way (a comment, a fabricated value) would be worse than an honest
zero. An operator tells the two apart by watching
`githearth_metrics_scrape_failures_total`: it is a counter, so `rate(...) > 0`
on any alerting interval means at least one recent scrape could not read its
source, and every database gauge in those scrapes is suspect. Alert on it.

A `5xx` is deliberately not used: Prometheus treats it as a failed scrape and
discards the body, which would hide both the failure counter and the four
gauges, and leave the counter unobservable.

## Related

- [Installation](install.md): the `doctor` checks that run before `serve`.
- [Housekeeping](housekeeping.md): what the housekeeping outcomes mean.
- [Storage quota](quota.md): per-repository usage, which this endpoint does not
  publish.
