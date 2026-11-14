use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

// Deliberately no Debug: database_url contains credentials.
#[derive(Clone, Deserialize)]
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
    /// IPs or CIDRs that are trusted to set `X-Forwarded-For` (TASK #88).
    /// Empty by default; every entry is validated at load.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Failed sign-ins allowed per username inside the window (M4.1 U03).
    #[serde(default = "default_login_throttle_max_per_username")]
    pub login_throttle_max_per_username: u64,
    /// Failed sign-ins allowed per client address inside the window.
    #[serde(default = "default_login_throttle_max_per_client")]
    pub login_throttle_max_per_client: u64,
    /// Length of the sliding sign-in window in seconds.
    #[serde(default = "default_login_throttle_window_seconds")]
    pub login_throttle_window_seconds: u64,
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
            hook_program: PathBuf::new(),
            secure_cookies: true,
            ssh_enabled: false,
            ssh_bind: "127.0.0.1:2222".parse().unwrap(),
            ssh_host_key_path: PathBuf::new(),
            job_concurrency: default_job_concurrency(),
            job_timeout_seconds: default_job_timeout_seconds(),
            job_poll_interval_ms: default_job_poll_interval_ms(),
            job_retention_days: default_job_retention_days(),
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
        }
    }
}
fn default_login_throttle_max_per_username() -> u64 {
    10
}
fn default_login_throttle_max_per_client() -> u64 {
    50
}
fn default_login_throttle_window_seconds() -> u64 {
    900
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
    30
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
            || !(1..=64).contains(&self.job_concurrency)
            || !(1..=3600).contains(&self.job_timeout_seconds)
            || !(50..=60000).contains(&self.job_poll_interval_ms)
            || !(1..=365).contains(&self.job_retention_days)
            || !(1..=1_000_000).contains(&self.login_throttle_max_per_username)
            || !(1..=1_000_000).contains(&self.login_throttle_max_per_client)
            || !(1..=86_400).contains(&self.login_throttle_window_seconds)
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
                "configuration bounds: pool 1..100, DB timeout 1..30s, request timeout 1..120s, token TTL 1..86400s, body 1024..1048576 bytes, auth concurrency 1..16, reconcile interval 5..3600s, reconcile grace 30..86400s, merge worker interval 1..3600s, trash retention 3600..31536000s, git concurrency 1..256, git timeout 10..86400s, git request limit 1024..1073741824 bytes, git push limit 1024..10737418240 bytes, repository quota bytes 0 or 1MiB..1TiB, job concurrency 1..64, job timeout 1..3600s, job poll 50..60000ms, job retention 1..365d, login throttle 1..1000000 failed sign-ins per username and per client inside a 1..86400s window, housekeeping interval 60..2592000s, housekeeping grace 0..31536000s, housekeeping concurrency 1..16, housekeeping timeout 10..86400s, housekeeping live-set cap 1MiB..1GiB, housekeeping pin retention 1..3650d, housekeeping fork fetch limit 1MiB..1GiB, housekeeping job budget (7*timeout+60s) <= 604860s"
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
        assert_eq!(defaults.housekeeping_pin_retention_days, 30);
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
        assert_eq!(c.login_throttle_max_per_username, 10);
        assert_eq!(c.login_throttle_max_per_client, 50);
        assert_eq!(c.login_throttle_window_seconds, 900);
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
