use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

// Deliberately no Debug: database_url contains credentials.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,
    pub database_max_connections: u32,
    pub database_timeout_seconds: u64,
    pub request_timeout_seconds: u64,
    pub token_ttl_seconds: u64,
    pub body_limit_bytes: usize,
    pub auth_concurrency: usize,
    pub storage_root: PathBuf,
    pub reconcile_interval_seconds: u64,
    pub reconcile_grace_seconds: u64,
    pub merge_worker_interval_seconds: u64,
    pub trash_retention_seconds: u64,
    pub git_concurrency: usize,
    pub git_timeout_seconds: u64,
    pub git_request_limit_bytes: usize,
    pub git_push_limit_bytes: usize,
    pub repository_quota_bytes: u64,
    /// Per-organization byte limit across the object store. `0` disables it,
    /// matching `repository_quota_bytes`. Objects are charged to the
    /// organization that owns their tenant prefix, and the bytes of an open
    /// multipart upload count until the upload is completed or swept.
    #[serde(default)]
    pub organization_quota_bytes: u64,
    /// How long a multipart upload may sit without activity before the
    /// reconciler removes it, in seconds (ADR 0014 §1).
    #[serde(default = "default_object_upload_deadline_seconds")]
    pub object_upload_deadline_seconds: u64,
    /// Path of the binary the Git hooks `exec`; empty means the current executable.
    #[serde(default)]
    pub hook_program: PathBuf,
    #[serde(default = "default_secure_cookies")]
    pub secure_cookies: bool,
    #[serde(default)]
    pub ssh_enabled: bool,
    #[serde(default = "default_ssh_bind")]
    pub ssh_bind: SocketAddr,
    #[serde(default)]
    pub ssh_host_key_path: PathBuf,
    /// Number of concurrent job handlers (ADR 0006).
    #[serde(default = "default_job_concurrency")]
    pub job_concurrency: usize,
    /// Per-job handler timeout in seconds (ADR 0006).
    #[serde(default = "default_job_timeout_seconds")]
    pub job_timeout_seconds: u64,
    /// Poll interval in ms when no work is found (ADR 0006).
    #[serde(default = "default_job_poll_interval_ms")]
    pub job_poll_interval_ms: u64,
    /// Whether scheduled repository housekeeping runs (ADR 0013).
    #[serde(default = "default_housekeeping_enabled")]
    pub housekeeping_enabled: bool,
    /// How often each repository is due for housekeeping, in seconds.
    #[serde(default = "default_housekeeping_interval_seconds")]
    pub housekeeping_interval_seconds: u64,
    /// Minimum age of an unreachable object before it may be deleted.
    #[serde(default = "default_housekeeping_grace_seconds")]
    pub housekeeping_grace_seconds: u64,
    /// How many housekeeping runs may be in flight at once.
    #[serde(default = "default_housekeeping_concurrency")]
    pub housekeeping_concurrency: usize,
    /// Per-step Git timeout for a housekeeping run, in seconds.
    #[serde(default = "default_housekeeping_timeout_seconds")]
    pub housekeeping_timeout_seconds: u64,
    /// Largest `rev-list --objects --all` output a run will accept. A larger
    /// walk refuses to prune rather than using a partial live set.
    #[serde(default = "default_housekeeping_max_live_set_bytes")]
    pub housekeeping_max_live_set_bytes: usize,
    /// How long a closed or merged pull request's head, base and superseded
    /// status pins stay live, in days. A pin is what keeps the diff readable
    /// after the branch goes.
    #[serde(default = "default_housekeeping_pin_retention_days")]
    pub housekeeping_pin_retention_days: u64,
    /// The most bytes one fork pull request's head may add to the target
    /// repository. The objects are copied into the target (which is what
    /// makes `compare` and mergeability resolve them there) and charged to
    /// the target organization; a fetch that would exceed this is refused
    /// before it runs, and the pull request still works from the fork.
    #[serde(default = "default_housekeeping_fork_fetch_limit_bytes")]
    pub housekeeping_fork_fetch_limit_bytes: u64,
    /// Completed jobs retention in days (ADR 0006).
    #[serde(default = "default_job_retention_days")]
    pub job_retention_days: u64,
    /// Outbox fan-out poll interval in ms when no undelivered event is found
    /// (ADR 0006, M4.1).
    #[serde(default = "default_outbox_poll_interval_ms")]
    pub outbox_poll_interval_ms: u64,
    /// IPs or CIDRs that are trusted to set `X-Forwarded-For` (TASK #88).
    /// Empty by default; every entry is validated at load.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Failed sign-ins allowed per username inside the window (M4.1 U03/U18).
    #[serde(default = "default_login_throttle_max_per_username")]
    pub login_throttle_max_per_username: u64,
    /// Failed sign-ins allowed per client address inside the window.
    #[serde(default = "default_login_throttle_max_per_client")]
    pub login_throttle_max_per_client: u64,
    /// Length of the sliding sign-in window in seconds.
    #[serde(default = "default_login_throttle_window_seconds")]
    pub login_throttle_window_seconds: u64,
    /// The first step of the exponential delay imposed after the fifth
    /// recorded failure inside the window, in milliseconds. Each further
    /// failure doubles it up to `login_throttle_delay_max_ms`.
    #[serde(default = "default_login_throttle_delay_base_ms")]
    pub login_throttle_delay_base_ms: u64,
    /// The longest delay one password attempt can wait, in milliseconds.
    /// Bounded so a refusal never holds a connection indefinitely.
    #[serde(default = "default_login_throttle_delay_max_ms")]
    pub login_throttle_delay_max_ms: u64,
    /// Answer `GET /metrics` (M4.1 U14). Off by default: until an operator asks
    /// for it the handler refuses every scrape, loopback or not.
    #[serde(default)]
    pub metrics_enabled: bool,
    /// Answer `GET /metrics` to a client that is not loopback after all. The
    /// default refusal is a `404`; turning this on exposes the scrape to every
    /// client that can reach the listener, so prefer proxying a scrape to a
    /// loopback address.
    #[serde(default)]
    pub metrics_allow_non_loopback: bool,
    /// Outbound SMTP host. Empty (the default) disables email delivery; the
    /// verification link and notifications then queue and retry, but no socket
    /// is opened. See docs/operations/email.md.
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_tls: SmtpTls,
    /// The envelope sender and `From:` address. Required when `smtp_host` is set.
    #[serde(default)]
    pub smtp_from: String,
    /// SMTP AUTH username. Authentication is skipped when both this and the
    /// credential file are absent (an unauthenticated relay).
    #[serde(default)]
    pub smtp_username: Option<String>,
    /// File holding the SMTP credential. Read once at start-up and never
    /// logged, never returned, never placed in an error message. `-` reads one
    /// line from stdin. The credential is never a TOML value.
    #[serde(default)]
    pub smtp_password_file: Option<PathBuf>,
    /// Absolute base URL used in mail links. No trailing slash is required.
    #[serde(default = "default_public_url")]
    pub public_url: String,
    /// Directory `backup` writes to when `--output` is not given.
    #[serde(default = "default_backup_directory")]
    pub backup_directory: PathBuf,
    /// Key file `backup`/`restore` read when `--key-file` is not given. The
    /// file's contents are the key: the path is configuration, the key is not.
    #[serde(default)]
    pub backup_key_file: PathBuf,
    /// How long `maintenance start` waits for in-flight writes to drain.
    #[serde(default = "default_maintenance_drain_timeout_seconds")]
    pub maintenance_drain_timeout_seconds: u64,
}

/// How the SMTP connection is protected.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS (the default, usually port 587).
    #[default]
    Starttls,
    /// TLS from the first byte (usually port 465).
    Implicit,
    /// Cleartext. Only for a loopback relay in development, and refused when
    /// an SMTP username is configured: AUTH over it would send the password
    /// in the clear.
    None,
}

impl std::str::FromStr for SmtpTls {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "starttls" => Ok(Self::Starttls),
            "implicit" => Ok(Self::Implicit),
            "none" => Ok(Self::None),
            _ => Err("smtp_tls must be starttls, implicit or none"),
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            bind: "127.0.0.1:8080".parse().unwrap(),
            database_max_connections: 10,
            database_timeout_seconds: 3,
            request_timeout_seconds: 15,
            token_ttl_seconds: 3600,
            body_limit_bytes: 524_288,
            auth_concurrency: 2,
            storage_root: PathBuf::from("data/repositories"),
            reconcile_interval_seconds: 60,
            reconcile_grace_seconds: 300,
            merge_worker_interval_seconds: 2,
            trash_retention_seconds: 604_800,
            git_concurrency: 8,
            git_timeout_seconds: 600,
            git_request_limit_bytes: 10_485_760,
            git_push_limit_bytes: 1_073_741_824,
            repository_quota_bytes: 0,
            organization_quota_bytes: 0,
            object_upload_deadline_seconds: default_object_upload_deadline_seconds(),
            hook_program: PathBuf::new(),
            secure_cookies: true,
            ssh_enabled: false,
            ssh_bind: "127.0.0.1:2222".parse().unwrap(),
            ssh_host_key_path: PathBuf::new(),
            job_concurrency: default_job_concurrency(),
            job_timeout_seconds: default_job_timeout_seconds(),
            job_poll_interval_ms: default_job_poll_interval_ms(),
            job_retention_days: default_job_retention_days(),
            outbox_poll_interval_ms: default_outbox_poll_interval_ms(),
            housekeeping_enabled: default_housekeeping_enabled(),
            housekeeping_interval_seconds: default_housekeeping_interval_seconds(),
            housekeeping_grace_seconds: default_housekeeping_grace_seconds(),
            housekeeping_concurrency: default_housekeeping_concurrency(),
            housekeeping_timeout_seconds: default_housekeeping_timeout_seconds(),
            housekeeping_max_live_set_bytes: default_housekeeping_max_live_set_bytes(),
            housekeeping_pin_retention_days: default_housekeeping_pin_retention_days(),
            housekeeping_fork_fetch_limit_bytes: default_housekeeping_fork_fetch_limit_bytes(),
            trusted_proxies: Vec::new(),
            login_throttle_max_per_username: default_login_throttle_max_per_username(),
            login_throttle_max_per_client: default_login_throttle_max_per_client(),
            login_throttle_window_seconds: default_login_throttle_window_seconds(),
            login_throttle_delay_base_ms: default_login_throttle_delay_base_ms(),
            login_throttle_delay_max_ms: default_login_throttle_delay_max_ms(),
            metrics_enabled: false,
            metrics_allow_non_loopback: false,
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            smtp_tls: SmtpTls::Starttls,
            smtp_from: String::new(),
            smtp_username: None,
            smtp_password_file: None,
            public_url: default_public_url(),
            backup_directory: default_backup_directory(),
            backup_key_file: PathBuf::new(),
            maintenance_drain_timeout_seconds: default_maintenance_drain_timeout_seconds(),
        }
    }
}
fn default_smtp_port() -> u16 {
    587
}
fn default_public_url() -> String {
    "http://127.0.0.1:8080".to_string()
}
fn default_backup_directory() -> PathBuf {
    PathBuf::from("backups")
}
fn default_maintenance_drain_timeout_seconds() -> u64 {
    300
}
fn default_object_upload_deadline_seconds() -> u64 {
    86_400
}
fn default_login_throttle_max_per_username() -> u64 {
    5
}
fn default_login_throttle_max_per_client() -> u64 {
    20
}
fn default_login_throttle_window_seconds() -> u64 {
    900
}
fn default_login_throttle_delay_base_ms() -> u64 {
    200
}
fn default_login_throttle_delay_max_ms() -> u64 {
    2_000
}
fn default_secure_cookies() -> bool {
    true
}
fn default_ssh_bind() -> SocketAddr {
    "127.0.0.1:2222".parse().unwrap()
}
fn default_job_concurrency() -> usize {
    4
}
fn default_job_timeout_seconds() -> u64 {
    60
}
fn default_job_poll_interval_ms() -> u64 {
    500
}
fn default_outbox_poll_interval_ms() -> u64 {
    500
}

fn default_job_retention_days() -> u64 {
    7
}
fn default_housekeeping_enabled() -> bool {
    true
}
fn default_housekeeping_interval_seconds() -> u64 {
    86_400
}
fn default_housekeeping_grace_seconds() -> u64 {
    691_200
}
fn default_housekeeping_concurrency() -> usize {
    2
}
fn default_housekeeping_timeout_seconds() -> u64 {
    600
}
fn default_housekeeping_max_live_set_bytes() -> usize {
    1 << 26
}
fn default_housekeeping_pin_retention_days() -> u64 {
    90
}
fn default_housekeeping_fork_fetch_limit_bytes() -> u64 {
    1 << 26
}

/// The bounded phases of one housekeeping run, each covered by one
/// `housekeeping_timeout_seconds`:
///
/// 1. refresh the open pull-request pins (the journal replay before it is
///    bounded per Git step, not by this budget),
/// 2. the pre-repack live-set walk,
/// 3. the size walk before the snapshot,
/// 4. the snapshot (`repack`),
/// 5. the post-repack live-set walk,
/// 6. the scan and deletion pass,
/// 7. the verification (`fsck` and the final size walk share it).
///
/// The job budget is this many step timeouts plus slack.
pub const HOUSEKEEPING_STEPS: u64 = 7;
/// Extra time for the parts of a run that are not a single Git command.
pub const HOUSEKEEPING_JOB_SLACK_SECONDS: u64 = 60;
/// The largest step timeout `validate` accepts. The budget maximum is derived
/// from it rather than stated separately: three numbers that must agree (the
/// step count, the slack and the budget maximum) are two numbers, so the
/// arithmetic cannot drift again.
pub const MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS: u64 = 86_400;
/// No job budget (and therefore no lease) may exceed
/// [`MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS`] worth of steps plus slack, so a
/// stuck lease is always recoverable long before job retention:
/// `7 * 86400 + 60 = 604_860` seconds (about 7 days and 1 minute), and the
/// claim lease (twice the budget) is at most `1_209_720` seconds, about 14
/// days, inside the 365-day job retention.
pub const MAX_JOB_BUDGET_SECONDS: u64 =
    MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS * HOUSEKEEPING_STEPS + HOUSEKEEPING_JOB_SLACK_SECONDS;

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut c: Self = match path {
            Some(path) => toml::from_str(
                &std::fs::read_to_string(path).context("cannot read configuration file")?,
            )
            .map_err(|_| {
                anyhow::anyhow!("invalid TOML configuration: check field names and types")
            })?,
            None => Self::default(),
        };
        macro_rules! env {
            ($field:ident, $name:literal) => {
                if let Ok(value) = std::env::var($name) {
                    c.$field = value
                        .parse()
                        .map_err(|_| anyhow::anyhow!(concat!("invalid ", $name)))?;
                }
            };
        }
        env!(database_url, "GITHEARTH_DATABASE_URL");
        env!(bind, "GITHEARTH_BIND");
        env!(
            database_max_connections,
            "GITHEARTH_DATABASE_MAX_CONNECTIONS"
        );
        env!(
            database_timeout_seconds,
            "GITHEARTH_DATABASE_TIMEOUT_SECONDS"
        );
        env!(request_timeout_seconds, "GITHEARTH_REQUEST_TIMEOUT_SECONDS");
        env!(token_ttl_seconds, "GITHEARTH_TOKEN_TTL_SECONDS");
        env!(body_limit_bytes, "GITHEARTH_BODY_LIMIT_BYTES");
        env!(auth_concurrency, "GITHEARTH_AUTH_CONCURRENCY");
        env!(secure_cookies, "GITHEARTH_SECURE_COOKIES");
        env!(git_concurrency, "GITHEARTH_GIT_CONCURRENCY");
        env!(git_timeout_seconds, "GITHEARTH_GIT_TIMEOUT_SECONDS");
        env!(git_request_limit_bytes, "GITHEARTH_GIT_REQUEST_LIMIT_BYTES");
        env!(git_push_limit_bytes, "GITHEARTH_GIT_PUSH_LIMIT_BYTES");
        env!(repository_quota_bytes, "GITHEARTH_REPOSITORY_QUOTA_BYTES");
        env!(
            organization_quota_bytes,
            "GITHEARTH_ORGANIZATION_QUOTA_BYTES"
        );
        env!(
            object_upload_deadline_seconds,
            "GITHEARTH_OBJECT_UPLOAD_DEADLINE_SECONDS"
        );
        env!(ssh_enabled, "GITHEARTH_SSH_ENABLED");
        if std::env::var("GITHEARTH_SSH_BIND")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            env!(ssh_bind, "GITHEARTH_SSH_BIND");
        }
        if std::env::var("GITHEARTH_SSH_HOST_KEY_PATH")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            env!(ssh_host_key_path, "GITHEARTH_SSH_HOST_KEY_PATH");
        }
        if std::env::var("GITHEARTH_HOOK_PROGRAM")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            env!(hook_program, "GITHEARTH_HOOK_PROGRAM");
        }
        if std::env::var("GITHEARTH_STORAGE_ROOT")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            env!(storage_root, "GITHEARTH_STORAGE_ROOT");
        }
        env!(
            reconcile_interval_seconds,
            "GITHEARTH_RECONCILE_INTERVAL_SECONDS"
        );
        env!(trash_retention_seconds, "GITHEARTH_TRASH_RETENTION_SECONDS");
        env!(reconcile_grace_seconds, "GITHEARTH_RECONCILE_GRACE_SECONDS");
        env!(
            merge_worker_interval_seconds,
            "GITHEARTH_MERGE_WORKER_INTERVAL_SECONDS"
        );
        env!(job_concurrency, "GITHEARTH_JOB_CONCURRENCY");
        env!(job_timeout_seconds, "GITHEARTH_JOB_TIMEOUT_SECONDS");
        env!(job_poll_interval_ms, "GITHEARTH_JOB_POLL_INTERVAL_MS");
        env!(job_retention_days, "GITHEARTH_JOB_RETENTION_DAYS");
        env!(outbox_poll_interval_ms, "GITHEARTH_OUTBOX_POLL_INTERVAL_MS");
        env!(
            login_throttle_max_per_username,
            "GITHEARTH_LOGIN_THROTTLE_MAX_PER_USERNAME"
        );
        env!(
            login_throttle_max_per_client,
            "GITHEARTH_LOGIN_THROTTLE_MAX_PER_CLIENT"
        );
        env!(
            login_throttle_window_seconds,
            "GITHEARTH_LOGIN_THROTTLE_WINDOW_SECONDS"
        );
        env!(
            login_throttle_delay_base_ms,
            "GITHEARTH_LOGIN_THROTTLE_DELAY_BASE_MS"
        );
        env!(
            login_throttle_delay_max_ms,
            "GITHEARTH_LOGIN_THROTTLE_DELAY_MAX_MS"
        );
        env!(metrics_enabled, "GITHEARTH_METRICS_ENABLED");
        env!(
            metrics_allow_non_loopback,
            "GITHEARTH_METRICS_ALLOW_NON_LOOPBACK"
        );
        env!(smtp_host, "GITHEARTH_SMTP_HOST");
        env!(smtp_port, "GITHEARTH_SMTP_PORT");
        env!(smtp_tls, "GITHEARTH_SMTP_TLS");
        env!(smtp_from, "GITHEARTH_SMTP_FROM");
        if let Ok(value) = std::env::var("GITHEARTH_SMTP_USERNAME") {
            c.smtp_username = (!value.is_empty()).then_some(value);
        }
        if let Ok(value) = std::env::var("GITHEARTH_SMTP_PASSWORD_FILE") {
            c.smtp_password_file = (!value.is_empty()).then(|| PathBuf::from(value));
        }
        env!(public_url, "GITHEARTH_PUBLIC_URL");
        if std::env::var("GITHEARTH_BACKUP_DIRECTORY")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            env!(backup_directory, "GITHEARTH_BACKUP_DIRECTORY");
        }
        if std::env::var("GITHEARTH_BACKUP_KEY_FILE")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            env!(backup_key_file, "GITHEARTH_BACKUP_KEY_FILE");
        }
        env!(
            maintenance_drain_timeout_seconds,
            "GITHEARTH_MAINTENANCE_DRAIN_TIMEOUT_SECONDS"
        );
        env!(housekeeping_enabled, "GITHEARTH_HOUSEKEEPING_ENABLED");
        env!(
            housekeeping_interval_seconds,
            "GITHEARTH_HOUSEKEEPING_INTERVAL_SECONDS"
        );
        env!(
            housekeeping_grace_seconds,
            "GITHEARTH_HOUSEKEEPING_GRACE_SECONDS"
        );
        env!(
            housekeeping_concurrency,
            "GITHEARTH_HOUSEKEEPING_CONCURRENCY"
        );
        env!(
            housekeeping_timeout_seconds,
            "GITHEARTH_HOUSEKEEPING_TIMEOUT_SECONDS"
        );
        env!(
            housekeeping_max_live_set_bytes,
            "GITHEARTH_HOUSEKEEPING_MAX_LIVE_SET_BYTES"
        );
        env!(
            housekeeping_pin_retention_days,
            "GITHEARTH_HOUSEKEEPING_PIN_RETENTION_DAYS"
        );
        env!(
            housekeeping_fork_fetch_limit_bytes,
            "GITHEARTH_HOUSEKEEPING_FORK_FETCH_LIMIT_BYTES"
        );
        if let Ok(value) = std::env::var("GITHEARTH_TRUSTED_PROXIES") {
            c.trusted_proxies = value
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        c.validate()?;
        Ok(c)
    }

    /// The `repository.housekeeping` job's own run budget. One housekeeping
    /// run has [`HOUSEKEEPING_STEPS`] bounded phases, so the generic
    /// `job_timeout_seconds` (60 s by default) cannot be its limit: a run cut
    /// off mid-step records no result and its blocking Git work outlives the
    /// job.
    pub fn housekeeping_job_timeout_seconds(&self) -> u64 {
        self.housekeeping_timeout_seconds
            .saturating_mul(HOUSEKEEPING_STEPS)
            .saturating_add(HOUSEKEEPING_JOB_SLACK_SECONDS)
    }

    /// The smallest budget that covers every bounded phase of a run. Validation
    /// refuses a configuration whose budget is below this, so a setting the
    /// code cannot honour is never accepted.
    pub fn min_housekeeping_job_budget_seconds(&self) -> u64 {
        self.housekeeping_timeout_seconds
            .saturating_mul(HOUSEKEEPING_STEPS)
            .saturating_add(HOUSEKEEPING_JOB_SLACK_SECONDS)
    }

    pub fn validate(&self) -> Result<()> {
        let housekeeping_budget = self.housekeeping_job_timeout_seconds();
        if !(self.database_url.starts_with("postgres://")
            || self.database_url.starts_with("postgresql://"))
        {
            bail!("database_url must be a PostgreSQL URL");
        }
        if !(1..=100).contains(&self.database_max_connections)
            || !(1..=30).contains(&self.database_timeout_seconds)
            || !(1..=120).contains(&self.request_timeout_seconds)
            || !(1..=86400).contains(&self.token_ttl_seconds)
            || !(1024..=1048576).contains(&self.body_limit_bytes)
            || !(1..=16).contains(&self.auth_concurrency)
            || !(5..=3600).contains(&self.reconcile_interval_seconds)
            || !(30..=86400).contains(&self.reconcile_grace_seconds)
            || !(1..=3600).contains(&self.merge_worker_interval_seconds)
            || !(3600..=31536000).contains(&self.trash_retention_seconds)
            || !(1..=256).contains(&self.git_concurrency)
            || !(10..=86400).contains(&self.git_timeout_seconds)
            || !(1024..=1073741824).contains(&self.git_request_limit_bytes)
            || !(1024..=10737418240).contains(&self.git_push_limit_bytes)
            || (self.repository_quota_bytes != 0
                && !(1_048_576..=1_099_511_627_776).contains(&self.repository_quota_bytes))
            || (self.organization_quota_bytes != 0
                && !(1_048_576..=1_099_511_627_776).contains(&self.organization_quota_bytes))
            || !(60..=2_592_000).contains(&self.object_upload_deadline_seconds)
            || !(1..=64).contains(&self.job_concurrency)
            || !(1..=3600).contains(&self.job_timeout_seconds)
            || !(50..=60000).contains(&self.job_poll_interval_ms)
            || !(1..=365).contains(&self.job_retention_days)
            || !(50..=60000).contains(&self.outbox_poll_interval_ms)
            || !(1..=1_000_000).contains(&self.login_throttle_max_per_username)
            || !(1..=1_000_000).contains(&self.login_throttle_max_per_client)
            || !(1..=86_400).contains(&self.login_throttle_window_seconds)
            || !(1..=60_000).contains(&self.login_throttle_delay_base_ms)
            || !(1..=600_000).contains(&self.login_throttle_delay_max_ms)
            || self.login_throttle_delay_max_ms < self.login_throttle_delay_base_ms
            || self.backup_directory.as_os_str().is_empty()
            || !(1..=86_400).contains(&self.maintenance_drain_timeout_seconds)
            || !(60..=2_592_000).contains(&self.housekeeping_interval_seconds)
            || self.housekeeping_grace_seconds > 31_536_000
            || !(1..=16).contains(&self.housekeeping_concurrency)
            || !(10..=MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS).contains(&self.housekeeping_timeout_seconds)
            || !(1_048_576..=1_073_741_824).contains(&self.housekeeping_max_live_set_bytes)
            || !(1..=3650).contains(&self.housekeeping_pin_retention_days)
            || !(1_048_576..=1_073_741_824).contains(&self.housekeeping_fork_fetch_limit_bytes)
            // The per-kind job budget covers every bounded phase plus slack,
            // and its lease is twice that; keep both within a recoverable
            // maximum.
            || housekeeping_budget < self.min_housekeeping_job_budget_seconds()
            || housekeeping_budget > MAX_JOB_BUDGET_SECONDS
        {
            bail!(
                "configuration bounds: pool 1..100, DB timeout 1..30s, request timeout 1..120s, token TTL 1..86400s, body 1024..1048576 bytes, auth concurrency 1..16, reconcile interval 5..3600s, reconcile grace 30..86400s, merge worker interval 1..3600s, trash retention 3600..31536000s, git concurrency 1..256, git timeout 10..86400s, git request limit 1024..1073741824 bytes, git push limit 1024..10737418240 bytes, repository quota bytes 0 or 1MiB..1TiB, organization quota bytes 0 or 1MiB..1TiB, object upload deadline 60..2592000s, job concurrency 1..64, job timeout 1..3600s, job poll 50..60000ms, job retention 1..365d, outbox poll 50..60000ms, login throttle 1..1000000 failed sign-ins per username and per client inside a 1..86400s window, login throttle delay base 1..60000ms and max 1..600000ms (max >= base), backup directory must not be empty, maintenance drain timeout 1..86400s, housekeeping interval 60..2592000s, housekeeping grace 0..31536000s, housekeeping concurrency 1..16, housekeeping timeout 10..86400s, housekeeping live-set cap 1MiB..1GiB, housekeeping pin retention 1..3650d, housekeeping fork fetch limit 1MiB..1GiB, housekeeping job budget (7*timeout+60s) <= 604860s"
            );
        }
        if self.storage_root.as_os_str().is_empty() {
            bail!("storage_root must not be empty");
        }
        // The hook admission socket is <storage_root>/run/hook-XXXXXXXX.sock, and a
        // Unix socket path must fit sun_path (108 bytes with its NUL on Linux).
        let socket_len = self.storage_root.as_os_str().len() + HOOK_SOCKET_SUFFIX_LEN;
        if socket_len > MAX_SOCKET_PATH_LEN {
            bail!(
                "storage_root is too long: the hook socket path would be {socket_len} bytes, \
                 the Unix limit is {MAX_SOCKET_PATH_LEN}; use a shorter storage_root"
            );
        }
        if self.ssh_enabled && self.ssh_host_key_path.as_os_str().is_empty() {
            bail!("ssh_host_key_path must be set when ssh_enabled is true");
        }
        for entry in &self.trusted_proxies {
            if crate::client_ip::parse_trusted_proxy(entry).is_err() {
                bail!(
                    "trusted_proxies: invalid entry \"{entry}\"; expected IP address or CIDR range"
                );
            }
        }
        // Exposing the metrics endpoint to every client is a decision, so a
        // setting that does nothing without it is refused rather than silently
        // ignored.
        if self.metrics_allow_non_loopback && !self.metrics_enabled {
            bail!(
                "metrics_allow_non_loopback requires metrics_enabled = true; \
                 the metrics endpoint is off"
            );
        }
        if self.public_url.len() > 2048
            || !(self.public_url.starts_with("http://") || self.public_url.starts_with("https://"))
            || self
                .public_url
                .bytes()
                .any(|b| b.is_ascii_control() || b == b' ')
        {
            bail!("public_url must be an http:// or https:// URL without spaces or control bytes");
        }
        if !self.smtp_host.is_empty() {
            if self.smtp_port == 0 {
                bail!("smtp_port must be between 1 and 65535");
            }
            if !crate::invitations::valid_email(&self.smtp_from) {
                bail!("smtp_from must be a valid email address when smtp_host is set");
            }
            if self
                .smtp_username
                .as_deref()
                .is_some_and(|username| username.is_empty())
            {
                bail!("smtp_username must not be empty when set");
            }
            if self.smtp_username.is_some() && self.smtp_password_file.is_none() {
                bail!("smtp_username requires smtp_password_file");
            }
            // A cleartext connection with AUTH would put the password on the
            // wire in base64. `none` stays usable for an unauthenticated
            // loopback relay; an authenticated one must use TLS.
            if self.smtp_tls == SmtpTls::None && self.smtp_username.is_some() {
                bail!(
                    "smtp_tls = \"none\" cannot be combined with smtp_username: \
                     AUTH PLAIN would send the password in cleartext; use \
                     smtp_tls = \"starttls\" or \"implicit\""
                );
            }
        }
        Ok(())
    }
}

/// Length of `/run/hook-XXXXXXXX.sock` appended to the storage root.
const HOOK_SOCKET_SUFFIX_LEN: usize = "/run/hook-00000000.sock".len();
/// Longest Unix socket path: `sun_path` is 108 bytes including the NUL.
const MAX_SOCKET_PATH_LEN: usize = 107;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unknown_fields_and_redacts_toml_parse_errors() {
        let secret = "configuration-secret-must-not-appear";
        let path =
            std::env::temp_dir().join(format!("githearth-config-{}.toml", uuid::Uuid::new_v4()));
        let invalid_configs = [
            format!(
                "database_url = \"postgres://user:{secret}@localhost/test\"\nunknown_setting = true\n"
            ),
            format!(
                "database_url = \"postgres://localhost/test\"\ndatabase_max_connections = \"{secret}\"\n"
            ),
            format!("database_url = \"postgres://user:{secret}@localhost/test"),
        ];
        for config in invalid_configs {
            std::fs::write(&path, config).unwrap();
            let error = Config::load(Some(&path))
                .err()
                .expect("invalid configuration accepted");
            let message = format!("{error:#}");
            assert_eq!(
                message,
                "invalid TOML configuration: check field names and types"
            );
            assert!(!message.contains(secret));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_missing_database_and_unbounded_limits() {
        let mut c = Config::default();
        assert!(c.validate().is_err());
        c.database_url = "postgres://localhost/test".into();
        assert!(c.validate().is_ok());
        c.database_max_connections = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn housekeeping_job_budget_covers_every_phase() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        assert_eq!(
            c.housekeeping_job_timeout_seconds(),
            HOUSEKEEPING_STEPS * 600 + HOUSEKEEPING_JOB_SLACK_SECONDS
        );
        assert_eq!(
            c.housekeeping_job_timeout_seconds(),
            c.min_housekeeping_job_budget_seconds(),
            "the formula and the validated minimum are the same relation"
        );
        assert!(
            c.housekeeping_job_timeout_seconds() > 6 * c.housekeeping_timeout_seconds,
            "the budget covers more than the old four-phase count"
        );
        assert!(
            c.housekeeping_job_timeout_seconds() > c.job_timeout_seconds,
            "the kind's budget is longer than the generic timeout"
        );
        assert!(c.validate().is_ok());

        // The largest permitted step timeout still fits the budget maximum,
        // and the maximum is exactly the arithmetic of the largest one rather
        // than a separately stated number that can drift from it.
        assert_eq!(
            MAX_JOB_BUDGET_SECONDS,
            HOUSEKEEPING_STEPS * MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS
                + HOUSEKEEPING_JOB_SLACK_SECONDS
        );
        c.housekeeping_timeout_seconds = MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS;
        assert!(c.validate().is_ok());
        assert!(c.housekeeping_job_timeout_seconds() <= MAX_JOB_BUDGET_SECONDS);
        // One second above the largest permitted step timeout is refused,
        // as is a budget the maximum cannot cover.
        c.housekeeping_timeout_seconds = MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS + 1;
        assert!(c.validate().is_err());
        c.housekeeping_timeout_seconds = MAX_HOUSEKEEPING_STEP_TIMEOUT_SECONDS;

        c.housekeeping_timeout_seconds = 0; // below the 10 s bound
        assert!(c.validate().is_err());

        // The new knobs default to the documented values and are bounded.
        let defaults = Config::default();
        assert_eq!(defaults.housekeeping_max_live_set_bytes, 1 << 26);
        assert_eq!(defaults.housekeeping_pin_retention_days, 90);
        let mut invalid = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        invalid.housekeeping_max_live_set_bytes = 0;
        assert!(invalid.validate().is_err());
        invalid.housekeeping_max_live_set_bytes = 1 << 26;
        invalid.housekeeping_pin_retention_days = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn object_store_limits_default_to_unlimited_and_a_day_and_are_bounded() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        assert_eq!(c.organization_quota_bytes, 0, "unlimited by default");
        assert_eq!(c.object_upload_deadline_seconds, 86_400, "one day");
        assert!(c.validate().is_ok());
        c.organization_quota_bytes = 1;
        assert!(c.validate().is_err(), "a sub-1MiB quota is refused");
        c.organization_quota_bytes = 1_048_576;
        assert!(c.validate().is_ok());
        c.object_upload_deadline_seconds = 59;
        assert!(c.validate().is_err());
        c.object_upload_deadline_seconds = 60;
        assert!(c.validate().is_ok());
        c.object_upload_deadline_seconds = 2_592_000;
        assert!(c.validate().is_ok());
        c.object_upload_deadline_seconds = 2_592_001;
        assert!(c.validate().is_err());
    }

    #[test]
    fn storage_root_must_leave_room_for_the_hook_socket() {
        let fits = "/".to_owned() + &"a".repeat(MAX_SOCKET_PATH_LEN - HOOK_SOCKET_SUFFIX_LEN - 1);
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            storage_root: PathBuf::from(&fits),
            ..Config::default()
        };
        assert!(c.validate().is_ok(), "a root that just fits is accepted");
        c.storage_root = PathBuf::from(fits + "a");
        let error = c.validate().unwrap_err().to_string();
        assert!(error.contains("storage_root is too long"), "{error}");
    }

    #[test]
    fn login_throttle_defaults_are_bounded_and_configurable() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        // The plan's published defaults (04-forge-roadmap "Accounts", U18).
        assert_eq!(c.login_throttle_max_per_username, 5);
        assert_eq!(c.login_throttle_max_per_client, 20);
        assert_eq!(c.login_throttle_window_seconds, 900);
        assert_eq!(c.login_throttle_delay_base_ms, 200);
        assert_eq!(c.login_throttle_delay_max_ms, 2_000);
        assert!(c.validate().is_ok());
        for (limit, window) in [(0, 900), (10, 0), (10, 86_401), (1_000_001, 900)] {
            c.login_throttle_max_per_username = limit;
            c.login_throttle_window_seconds = window;
            assert!(c.validate().is_err(), "{limit} {window} must be refused");
        }
        c.login_throttle_max_per_username = 1;
        c.login_throttle_window_seconds = 1;
        c.login_throttle_max_per_client = 1;
        assert!(c.validate().is_ok(), "the smallest bounds are accepted");
        // The delay's cap must not be below its first step, or the setting
        // would silently do nothing but the cap.
        c.login_throttle_delay_base_ms = 500;
        c.login_throttle_delay_max_ms = 100;
        assert!(c.validate().is_err());
        c.login_throttle_delay_base_ms = 0;
        c.login_throttle_delay_max_ms = 100;
        assert!(c.validate().is_err());
        c.login_throttle_delay_base_ms = 100;
        c.login_throttle_delay_max_ms = 100;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn metrics_are_off_by_default_and_the_override_needs_them_on() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        assert!(!c.metrics_enabled, "the endpoint is off by default");
        assert!(!c.metrics_allow_non_loopback, "and loopback-only when on");
        assert!(c.validate().is_ok());
        c.metrics_enabled = true;
        assert!(c.validate().is_ok(), "loopback-only needs nothing else");
        // The override without the endpoint is a setting that does nothing.
        c.metrics_enabled = false;
        c.metrics_allow_non_loopback = true;
        let error = c.validate().unwrap_err().to_string();
        assert!(error.contains("metrics_allow_non_loopback"), "{error}");
        c.metrics_enabled = true;
        assert!(c.validate().is_ok());
    }

    /// The shipped example must not trap an operator: every commented setting
    /// it suggests, uncommented as written (with the preconditions its comment
    /// states), has to load. `metrics_allow_non_loopback` is the one that was
    /// shipped as `false` under an invariant that refused it.
    #[test]
    fn the_example_file_suggests_only_values_the_validator_accepts() {
        const EXAMPLE: &str = include_str!("../../../githearth.example.toml");
        // The metrics block, as an operator would uncomment it line by line.
        let metrics: Vec<&str> = EXAMPLE
            .lines()
            .filter_map(|line| line.trim().strip_prefix('#'))
            .map(str::trim)
            .filter(|line| line.starts_with("metrics_"))
            .collect();
        assert!(
            metrics.iter().any(|l| l.starts_with("metrics_enabled = ")),
            "the example no longer shows metrics_enabled: {metrics:?}"
        );
        let toml = metrics.join("\n");
        assert!(
            !toml.contains("metrics_allow_non_loopback = false"),
            "the example ships the flag as false, which the validator refuses"
        );
        // The block as written, the way an operator uncomments it.
        let mut config: Config = toml::from_str(&toml)
            .unwrap_or_else(|e| panic!("the example's metrics settings do not parse: {e}"));
        config.database_url = "postgres://localhost/test".into();
        assert!(
            config.validate().is_ok(),
            "uncommenting the example's metrics block does not load"
        );
        assert!(
            config.metrics_enabled && config.metrics_allow_non_loopback,
            "the example's block was not the override with the endpoint on: {toml}"
        );
    }

    /// B1: `smtp_tls = "none"` must never carry a credential. A cleartext
    /// session with a username reaches `AUTH PLAIN` and puts the password on
    /// the wire, so `validate` refuses the combination. `none` remains usable
    /// for an unauthenticated loopback relay. The test also pins the SMTP
    /// keys' other validation rules, which had no test.
    #[test]
    fn smtp_cleartext_cannot_carry_credentials() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            smtp_host: "127.0.0.1".into(),
            smtp_port: 25,
            smtp_tls: SmtpTls::None,
            smtp_from: "githearth@example.com".into(),
            ..Config::default()
        };
        // Revert: delete the `SmtpTls::None && smtp_username` bail above; this
        // configuration validates and the client sends AUTH PLAIN in cleartext.
        assert!(
            c.validate().is_ok(),
            "an unauthenticated cleartext loopback relay stays usable"
        );
        c.smtp_username = Some("githearth".into());
        c.smtp_password_file = Some(PathBuf::from("/etc/githearth/smtp-password"));
        let error = c.validate().unwrap_err().to_string();
        assert!(
            error.contains("smtp_tls = \"none\" cannot be combined with smtp_username"),
            "{error}"
        );
        // The same credentials over TLS are accepted.
        c.smtp_tls = SmtpTls::Starttls;
        assert!(c.validate().is_ok());

        // The other SMTP rules: a zero port, a missing sender, a username with
        // no credential file.
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            smtp_host: "smtp.example.com".into(),
            ..Config::default()
        };
        c.smtp_port = 0;
        assert!(c.validate().is_err());
        c.smtp_port = 587;
        assert!(c.validate().is_err(), "smtp_from is required");
        c.smtp_from = "not an address".into();
        assert!(c.validate().is_err());
        c.smtp_from = "githearth@example.com".into();
        c.smtp_username = Some("githearth".into());
        let error = c.validate().unwrap_err().to_string();
        assert!(
            error.contains("smtp_username requires smtp_password_file"),
            "{error}"
        );
        c.smtp_password_file = Some(PathBuf::from("/etc/githearth/smtp-password"));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ssh_enabled_requires_host_key_path() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ssh_enabled: true,
            ..Config::default()
        };
        assert!(
            c.validate().is_err(),
            "ssh_enabled without key path must fail"
        );
        c.ssh_host_key_path = PathBuf::from("/some/key");
        assert!(c.validate().is_ok());
        c.ssh_enabled = false;
        assert!(c.validate().is_ok(), "ssh_disabled allows empty key path");
    }
}
