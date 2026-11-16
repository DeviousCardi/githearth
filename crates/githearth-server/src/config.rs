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
    /// Every organization may hold at least this many Git slots when queued.
    #[serde(default = "default_git_organization_floor")]
    pub git_organization_floor: usize,
    /// No organization may hold more than this many Git slots, however idle
    /// the rest of the installation is. The fair share of the remaining slots
    /// is `git_concurrency / active organizations`, clamped to this and the
    /// floor.
    #[serde(default = "default_git_organization_ceiling")]
    pub git_organization_ceiling: usize,
    /// How long a Git request over its share waits for a slot before the
    /// transport refuses it with `Retry-After`. The scheduler wakes a waiter
    /// below its floor first, so waiting is bounded by running work, not by a
    /// busier organization's queue.
    #[serde(default = "default_git_wait_millis")]
    pub git_wait_millis: u64,
    pub git_timeout_seconds: u64,
    pub git_request_limit_bytes: usize,
    pub git_push_limit_bytes: usize,
    pub repository_quota_bytes: u64,
    /// Per-organization byte limit for Git repository storage and the object
    /// store. `0` disables it, matching `repository_quota_bytes`. The Git half
    /// is the sum of every active repository's `objects/` tree, checked on each
    /// push and merge; the object-store half charges the organization that owns
    /// a tenant prefix, and an open multipart upload's bytes count until the
    /// upload is completed or swept. The two ledgers do not overlap.
    #[serde(default)]
    pub organization_quota_bytes: u64,
    /// Authenticated API requests allowed per organization per minute.
    #[serde(default = "default_api_requests_per_organization")]
    pub api_requests_per_organization: u64,
    /// Authenticated API requests allowed per account per minute.
    #[serde(default = "default_api_requests_per_account")]
    pub api_requests_per_account: u64,
    /// Authenticated Git transport requests allowed per organization per
    /// minute. A clone is at least two requests (`info/refs` and
    /// `upload-pack`), so this is deliberately twice the API organization
    /// budget.
    #[serde(default = "default_git_requests_per_organization")]
    pub git_requests_per_organization: u64,
    /// Authenticated Git transport requests allowed per account per minute,
    /// the second half of ADR 0014 §5's "separate budgets for the API and for
    /// Git operations".
    #[serde(default = "default_git_requests_per_account")]
    pub git_requests_per_account: u64,
    /// Anonymous clone/fetch requests allowed per client address per minute.
    #[serde(default = "default_anonymous_fetch_per_address")]
    pub anonymous_fetch_per_address: u64,
    /// Anonymous clone/fetch requests allowed across the installation per
    /// minute, so one address cannot exhaust the public capacity.
    #[serde(default = "default_anonymous_fetch_global")]
    pub anonymous_fetch_global: u64,
    /// Anonymous login and search requests allowed per client address per
    /// minute. One budget covers both: [`crate::limits::Limits::allow_anonymous_search`]
    /// spends the same counters, so a search route inherits this number
    /// instead of adding a second one. No search route ships in M4.2 (M9 owns
    /// it), so `docs/operations/limits.md` publishes the row as sign-in only.
    #[serde(default = "default_anonymous_login_per_address")]
    pub anonymous_login_per_address: u64,
    /// Anonymous login requests allowed across the installation per minute,
    /// so a second address cannot make the login form an unbounded
    /// password-hashing amplifier.
    #[serde(default = "default_anonymous_login_global")]
    pub anonymous_login_global: u64,
    /// Anonymous security-mail requests (change of address and password
    /// recovery) allowed per client address per hour. Charged before the
    /// per-destination address budget, so one client cannot use distinct
    /// addresses to grow that map without bound.
    #[serde(default = "default_anonymous_security_per_address")]
    pub anonymous_security_per_address: u64,
    /// The installation-wide anonymous security-mail budget, so a client that
    /// changes address cannot exhaust the public mail capacity.
    #[serde(default = "default_anonymous_security_global")]
    pub anonymous_security_global: u64,
    /// Organizations one account may create per 24 hours.
    #[serde(default = "default_organization_creation_per_account")]
    pub organization_creation_per_account: u64,
    /// Organizations the installation may create per UTC day.
    #[serde(default = "default_organization_creation_per_installation")]
    pub organization_creation_per_installation: u64,
    /// Jobs one organization may run at once, a share of `job_concurrency`.
    #[serde(default = "default_job_organization_share")]
    pub job_organization_share: usize,
    /// Email sends allowed per organization per hour.
    #[serde(default = "default_email_per_organization")]
    pub email_per_organization: u64,
    /// Security mail (verification and recovery) allowed per address per hour.
    #[serde(default = "default_email_security_per_address")]
    pub email_security_per_address: u64,
    /// Sign-up attempts allowed per client address in the velocity window.
    /// The endpoint arrives with M11a; this is the limiter it will call.
    #[serde(default = "default_signup_velocity_per_address")]
    pub signup_velocity_per_address: u64,
    /// Length of the sign-up velocity window in seconds.
    #[serde(default = "default_signup_velocity_window_seconds")]
    pub signup_velocity_window_seconds: u64,
    /// The published abuse contact. `doctor` warns when mail is configured and
    /// this is empty; the takedown runbook is docs/operations/abuse.md.
    #[serde(default)]
    pub abuse_contact: String,
    /// The envelope sender bounces go to (VERP/return path). Falls back to
    /// `smtp_from` when empty; `doctor` reports a configured relay with no
    /// bounce address.
    #[serde(default)]
    pub smtp_bounce_address: String,
    /// The DKIM selector whose `<selector>._domainkey.<domain>` TXT record
    /// `doctor` checks.
    #[serde(default = "default_smtp_dkim_selector")]
    pub smtp_dkim_selector: String,
    /// Whether `doctor` resolves SPF, DKIM and DMARC TXT records. On by
    /// default; a test or an offline host can turn it off.
    #[serde(default = "default_email_dns_check_enabled")]
    pub email_dns_check_enabled: bool,
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
    /// Destinations the server's one outbound client may call (ADR 0014 §7,
    /// case U30c). An entry is `host`, `host:port` or `*.domain`; a host-only
    /// entry covers only the standard ports 80 and 443. Empty by default, so
    /// the server calls nothing until an operator names it. An IP-literal entry
    /// is refused: every destination is a DNS name.
    #[serde(default)]
    pub outbound_allowlist: Vec<String>,
    /// Permit loopback, link-local and private-range answers. Off by default.
    /// Cloud-metadata endpoints stay refused even when this is on; they have
    /// their own option so opening an internal network never exposes cloud
    /// credentials. SMTP uses the same option for its operator-configured
    /// relay, so a loopback development relay needs this on.
    #[serde(default)]
    pub outbound_allow_private: bool,
    /// Permit the known cloud-metadata endpoints by address. Off by default.
    #[serde(default)]
    pub outbound_allow_metadata: bool,
    /// Whole-request budget in seconds, covering DNS, every redirect and the
    /// body read.
    #[serde(default = "default_outbound_timeout_seconds")]
    pub outbound_timeout_seconds: u64,
    /// Largest response body the outbound client will read.
    #[serde(default = "default_outbound_max_response_bytes")]
    pub outbound_max_response_bytes: usize,
    /// Largest request body the outbound client will send.
    #[serde(default = "default_outbound_max_request_bytes")]
    pub outbound_max_request_bytes: usize,
    /// Same-host redirects followed before the request is refused.
    #[serde(default = "default_outbound_max_redirects")]
    pub outbound_max_redirects: usize,
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
    /// How `backup` takes the storage snapshot: `auto` probes the filesystem
    /// for copy-on-write cloning and uses the online path when it works;
    /// `fence` skips the probe and keeps the M4.1 maintenance-window fence.
    /// A failed probe also falls back to the fence, so `fence` exists for an
    /// operator who knows the filesystem and for the fallback test.
    #[serde(default = "default_backup_snapshot_mode")]
    pub backup_snapshot_mode: String,
    /// The operator-owned key that wraps every stored secret (ADR 0014 §3).
    /// The file, not its path, is the key: it must be mode 0600, outside the
    /// storage root, and is never written to PostgreSQL or a backup. Empty
    /// means no key is configured; an installation that already holds encrypted
    /// secrets then refuses to start.
    #[serde(default)]
    pub installation_key_file: PathBuf,
    /// Decrypt-only predecessor kept while a rotation re-wraps data keys.
    #[serde(default)]
    pub installation_key_previous_file: PathBuf,
    /// `file` (a key or passphrase descriptor file) or `kms`. A KMS adapter is
    /// an interface in ADR 0014 §3; this binary ships no vendor adapter and
    /// refuses to start rather than silently falling back to a file.
    #[serde(default = "default_installation_key_method")]
    pub installation_key_method: String,
    /// File holding the passphrase when the installation key descriptor uses
    /// `method = "passphrase"`.
    #[serde(default)]
    pub installation_key_passphrase_file: Option<PathBuf>,
    /// Operator custody for per-account and per-organization data keys
    /// (M11a, crypto-shredding). Each key is wrapped by the installation key;
    /// the directory is outside `storage_root` and is **not** in a backup, so
    /// deleting a key makes its secrets unreadable in older backups. Empty
    /// derives `data-keys` beside `installation_key_file`.
    #[serde(default)]
    pub data_key_directory: PathBuf,
    /// How long `maintenance start` waits for in-flight writes to drain.
    #[serde(default = "default_maintenance_drain_timeout_seconds")]
    pub maintenance_drain_timeout_seconds: u64,
    /// The last day a legacy two-scope `repo:read`/`repo:write` token is
    /// accepted, as `YYYY-MM-DD`. It is refused from the following midnight
    /// UTC (U27). `doctor` reports the date and how many legacy tokens remain.
    #[serde(default = "default_legacy_token_deprecation_date")]
    pub legacy_token_deprecation_date: String,
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
            git_organization_floor: default_git_organization_floor(),
            git_organization_ceiling: default_git_organization_ceiling(),
            git_wait_millis: default_git_wait_millis(),
            git_timeout_seconds: 600,
            git_request_limit_bytes: 10_485_760,
            git_push_limit_bytes: 1_073_741_824,
            repository_quota_bytes: default_repository_quota_bytes(),
            organization_quota_bytes: default_organization_quota_bytes(),
            api_requests_per_organization: default_api_requests_per_organization(),
            api_requests_per_account: default_api_requests_per_account(),
            git_requests_per_organization: default_git_requests_per_organization(),
            git_requests_per_account: default_git_requests_per_account(),
            anonymous_fetch_per_address: default_anonymous_fetch_per_address(),
            anonymous_fetch_global: default_anonymous_fetch_global(),
            anonymous_login_per_address: default_anonymous_login_per_address(),
            anonymous_login_global: default_anonymous_login_global(),
            anonymous_security_per_address: default_anonymous_security_per_address(),
            anonymous_security_global: default_anonymous_security_global(),
            organization_creation_per_account: default_organization_creation_per_account(),
            organization_creation_per_installation: default_organization_creation_per_installation(
            ),
            job_organization_share: default_job_organization_share(),
            email_per_organization: default_email_per_organization(),
            email_security_per_address: default_email_security_per_address(),
            signup_velocity_per_address: default_signup_velocity_per_address(),
            signup_velocity_window_seconds: default_signup_velocity_window_seconds(),
            abuse_contact: String::new(),
            smtp_bounce_address: String::new(),
            smtp_dkim_selector: default_smtp_dkim_selector(),
            email_dns_check_enabled: default_email_dns_check_enabled(),
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
            outbound_allowlist: Vec::new(),
            outbound_allow_private: false,
            outbound_allow_metadata: false,
            outbound_timeout_seconds: default_outbound_timeout_seconds(),
            outbound_max_response_bytes: default_outbound_max_response_bytes(),
            outbound_max_request_bytes: default_outbound_max_request_bytes(),
            outbound_max_redirects: default_outbound_max_redirects(),
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            smtp_tls: SmtpTls::Starttls,
            smtp_from: String::new(),
            smtp_username: None,
            smtp_password_file: None,
            public_url: default_public_url(),
            backup_directory: default_backup_directory(),
            backup_key_file: PathBuf::new(),
            backup_snapshot_mode: default_backup_snapshot_mode(),
            installation_key_file: PathBuf::new(),
            installation_key_previous_file: PathBuf::new(),
            installation_key_method: default_installation_key_method(),
            installation_key_passphrase_file: None,
            data_key_directory: PathBuf::new(),
            maintenance_drain_timeout_seconds: default_maintenance_drain_timeout_seconds(),
            legacy_token_deprecation_date: default_legacy_token_deprecation_date(),
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
fn default_backup_snapshot_mode() -> String {
    "auto".to_string()
}
fn default_installation_key_method() -> String {
    "file".to_string()
}
fn default_maintenance_drain_timeout_seconds() -> u64 {
    300
}
fn default_object_upload_deadline_seconds() -> u64 {
    86_400
}
fn default_git_organization_floor() -> usize {
    1
}
fn default_git_organization_ceiling() -> usize {
    // Half the default concurrency of eight. Measurement revised this from
    // six (ADR 0014 open item 6): with six, an incumbent could fill 6 of 8
    // slots before a peer arrived and the peer's first requests waited behind
    // it. At four the first mover can never hold more than half, and two
    // tenants at their measured share fill the installation without queueing.
    // `GitScheduler::new` also clamps any configured ceiling to `concurrency -
    // 1`, so no organization may take every slot on a small installation.
    4
}
fn default_git_wait_millis() -> u64 {
    1_000
}
fn default_repository_quota_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}
fn default_organization_quota_bytes() -> u64 {
    50 * 1024 * 1024 * 1024
}
fn default_api_requests_per_organization() -> u64 {
    300
}
fn default_api_requests_per_account() -> u64 {
    60
}
fn default_git_requests_per_organization() -> u64 {
    600
}
fn default_git_requests_per_account() -> u64 {
    120
}
fn default_anonymous_fetch_per_address() -> u64 {
    120
}
fn default_anonymous_fetch_global() -> u64 {
    2_000
}
fn default_anonymous_login_per_address() -> u64 {
    20
}
fn default_anonymous_login_global() -> u64 {
    300
}
fn default_anonymous_security_per_address() -> u64 {
    20
}
fn default_anonymous_security_global() -> u64 {
    300
}
fn default_organization_creation_per_account() -> u64 {
    3
}
fn default_organization_creation_per_installation() -> u64 {
    50
}
fn default_job_organization_share() -> usize {
    2
}
fn default_email_per_organization() -> u64 {
    200
}
fn default_email_security_per_address() -> u64 {
    5
}
fn default_signup_velocity_per_address() -> u64 {
    3
}
fn default_signup_velocity_window_seconds() -> u64 {
    3_600
}
fn default_smtp_dkim_selector() -> String {
    "default".to_owned()
}
fn default_email_dns_check_enabled() -> bool {
    true
}
fn default_legacy_token_deprecation_date() -> String {
    "2027-06-30".to_owned()
}

/// The announced legacy window (ADR 0014 section 2a). The configured date can
/// move the end earlier but never later, so a token cannot be kept alive by
/// editing configuration.
const LEGACY_DEPRECATION_START: &str = "2026-11-16";
const LEGACY_DEPRECATION_END: &str = "2027-06-30";

/// `YYYY-MM-DD`, a real calendar date inside the announced legacy window.
fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !digits(0..4) || !digits(5..7) || !digits(8..10) {
        return false;
    }
    let year: u32 = value[0..4].parse().unwrap_or(0);
    let month: u32 = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && (LEGACY_DEPRECATION_START..=LEGACY_DEPRECATION_END).contains(&value)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
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
fn default_outbound_timeout_seconds() -> u64 {
    30
}
fn default_outbound_max_response_bytes() -> usize {
    5_242_880
}
fn default_outbound_max_request_bytes() -> usize {
    1_048_576
}
fn default_outbound_max_redirects() -> usize {
    5
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
        env!(git_organization_floor, "GITHEARTH_GIT_ORGANIZATION_FLOOR");
        env!(
            git_organization_ceiling,
            "GITHEARTH_GIT_ORGANIZATION_CEILING"
        );
        env!(git_wait_millis, "GITHEARTH_GIT_WAIT_MILLIS");
        env!(git_timeout_seconds, "GITHEARTH_GIT_TIMEOUT_SECONDS");
        env!(git_request_limit_bytes, "GITHEARTH_GIT_REQUEST_LIMIT_BYTES");
        env!(git_push_limit_bytes, "GITHEARTH_GIT_PUSH_LIMIT_BYTES");
        env!(repository_quota_bytes, "GITHEARTH_REPOSITORY_QUOTA_BYTES");
        env!(
            organization_quota_bytes,
            "GITHEARTH_ORGANIZATION_QUOTA_BYTES"
        );
        env!(
            api_requests_per_organization,
            "GITHEARTH_API_REQUESTS_PER_ORGANIZATION"
        );
        env!(
            api_requests_per_account,
            "GITHEARTH_API_REQUESTS_PER_ACCOUNT"
        );
        env!(
            git_requests_per_organization,
            "GITHEARTH_GIT_REQUESTS_PER_ORGANIZATION"
        );
        env!(
            git_requests_per_account,
            "GITHEARTH_GIT_REQUESTS_PER_ACCOUNT"
        );
        env!(
            anonymous_fetch_per_address,
            "GITHEARTH_ANONYMOUS_FETCH_PER_ADDRESS"
        );
        env!(anonymous_fetch_global, "GITHEARTH_ANONYMOUS_FETCH_GLOBAL");
        env!(
            anonymous_login_per_address,
            "GITHEARTH_ANONYMOUS_LOGIN_PER_ADDRESS"
        );
        env!(anonymous_login_global, "GITHEARTH_ANONYMOUS_LOGIN_GLOBAL");
        env!(
            anonymous_security_per_address,
            "GITHEARTH_ANONYMOUS_SECURITY_PER_ADDRESS"
        );
        env!(
            anonymous_security_global,
            "GITHEARTH_ANONYMOUS_SECURITY_GLOBAL"
        );
        env!(
            organization_creation_per_account,
            "GITHEARTH_ORGANIZATION_CREATION_PER_ACCOUNT"
        );
        env!(
            organization_creation_per_installation,
            "GITHEARTH_ORGANIZATION_CREATION_PER_INSTALLATION"
        );
        env!(job_organization_share, "GITHEARTH_JOB_ORGANIZATION_SHARE");
        env!(email_per_organization, "GITHEARTH_EMAIL_PER_ORGANIZATION");
        env!(
            email_security_per_address,
            "GITHEARTH_EMAIL_SECURITY_PER_ADDRESS"
        );
        env!(
            signup_velocity_per_address,
            "GITHEARTH_SIGNUP_VELOCITY_PER_ADDRESS"
        );
        env!(
            signup_velocity_window_seconds,
            "GITHEARTH_SIGNUP_VELOCITY_WINDOW_SECONDS"
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
        if let Ok(value) = std::env::var("GITHEARTH_OUTBOUND_ALLOWLIST") {
            c.outbound_allowlist = value
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        env!(outbound_allow_private, "GITHEARTH_OUTBOUND_ALLOW_PRIVATE");
        env!(outbound_allow_metadata, "GITHEARTH_OUTBOUND_ALLOW_METADATA");
        env!(
            outbound_timeout_seconds,
            "GITHEARTH_OUTBOUND_TIMEOUT_SECONDS"
        );
        env!(
            outbound_max_response_bytes,
            "GITHEARTH_OUTBOUND_MAX_RESPONSE_BYTES"
        );
        env!(
            outbound_max_request_bytes,
            "GITHEARTH_OUTBOUND_MAX_REQUEST_BYTES"
        );
        env!(outbound_max_redirects, "GITHEARTH_OUTBOUND_MAX_REDIRECTS");
        env!(smtp_host, "GITHEARTH_SMTP_HOST");
        env!(smtp_port, "GITHEARTH_SMTP_PORT");
        env!(smtp_tls, "GITHEARTH_SMTP_TLS");
        env!(smtp_from, "GITHEARTH_SMTP_FROM");
        env!(abuse_contact, "GITHEARTH_ABUSE_CONTACT");
        env!(smtp_bounce_address, "GITHEARTH_SMTP_BOUNCE_ADDRESS");
        env!(smtp_dkim_selector, "GITHEARTH_SMTP_DKIM_SELECTOR");
        env!(email_dns_check_enabled, "GITHEARTH_EMAIL_DNS_CHECK_ENABLED");
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
        if let Ok(value) = std::env::var("GITHEARTH_BACKUP_SNAPSHOT_MODE") {
            c.backup_snapshot_mode = value;
        }
        if let Ok(value) = std::env::var("GITHEARTH_INSTALLATION_KEY_FILE") {
            c.installation_key_file = if value.is_empty() {
                PathBuf::new()
            } else {
                PathBuf::from(value)
            };
        }
        if let Ok(value) = std::env::var("GITHEARTH_INSTALLATION_KEY_PREVIOUS_FILE") {
            c.installation_key_previous_file = if value.is_empty() {
                PathBuf::new()
            } else {
                PathBuf::from(value)
            };
        }
        if let Ok(value) = std::env::var("GITHEARTH_INSTALLATION_KEY_METHOD") {
            c.installation_key_method = value;
        }
        if let Ok(value) = std::env::var("GITHEARTH_INSTALLATION_KEY_PASSPHRASE_FILE") {
            c.installation_key_passphrase_file = (!value.is_empty()).then(|| PathBuf::from(value));
        }
        if let Ok(value) = std::env::var("GITHEARTH_DATA_KEY_DIRECTORY") {
            c.data_key_directory = if value.is_empty() {
                PathBuf::new()
            } else {
                PathBuf::from(value)
            };
        }
        env!(
            maintenance_drain_timeout_seconds,
            "GITHEARTH_MAINTENANCE_DRAIN_TIMEOUT_SECONDS"
        );
        env!(
            legacy_token_deprecation_date,
            "GITHEARTH_LEGACY_TOKEN_DEPRECATION_DATE"
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
        if !valid_date(&self.legacy_token_deprecation_date) {
            bail!(
                "legacy_token_deprecation_date must be a calendar date in YYYY-MM-DD \
                 between 2026-11-16 (the deprecation start) and 2027-06-30 (the \
                 announced end); it can shorten the legacy window, never extend it"
            );
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
            || !(1..=256).contains(&self.git_organization_floor)
            || !(1..=256).contains(&self.git_organization_ceiling)
            || self.git_organization_floor > self.git_concurrency
            || self.git_organization_ceiling < self.git_organization_floor
            || !(1..=60_000).contains(&self.git_wait_millis)
            || !(10..=86400).contains(&self.git_timeout_seconds)
            || !(1024..=1073741824).contains(&self.git_request_limit_bytes)
            || !(1024..=10737418240).contains(&self.git_push_limit_bytes)
            || (self.repository_quota_bytes != 0
                && !(1_048_576..=1_099_511_627_776).contains(&self.repository_quota_bytes))
            || (self.organization_quota_bytes != 0
                && !(1_048_576..=1_099_511_627_776).contains(&self.organization_quota_bytes))
            || (self.organization_quota_bytes != 0
                && self.repository_quota_bytes > self.organization_quota_bytes)
            || !(1..=1_000_000).contains(&self.api_requests_per_organization)
            || !(1..=1_000_000).contains(&self.api_requests_per_account)
            || !(1..=1_000_000).contains(&self.git_requests_per_organization)
            || !(1..=1_000_000).contains(&self.git_requests_per_account)
            || !(1..=1_000_000).contains(&self.anonymous_fetch_per_address)
            || !(1..=10_000_000).contains(&self.anonymous_fetch_global)
            || !(1..=1_000_000).contains(&self.anonymous_login_per_address)
            || !(1..=10_000_000).contains(&self.anonymous_login_global)
            || !(1..=1_000_000).contains(&self.anonymous_security_per_address)
            || !(1..=10_000_000).contains(&self.anonymous_security_global)
            || !(1..=1_000_000).contains(&self.organization_creation_per_account)
            || !(1..=1_000_000).contains(&self.organization_creation_per_installation)
            || !(1..=64).contains(&self.job_organization_share)
            || !(1..=1_000_000).contains(&self.email_per_organization)
            || !(1..=1_000_000).contains(&self.email_security_per_address)
            || !(1..=1_000_000).contains(&self.signup_velocity_per_address)
            || !(1..=86_400).contains(&self.signup_velocity_window_seconds)
            || self.smtp_dkim_selector.len() > 255
            || self.smtp_dkim_selector.contains(char::is_whitespace)
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
                "configuration bounds: pool 1..100, DB timeout 1..30s, request timeout 1..120s, token TTL 1..86400s, body 1024..1048576 bytes, auth concurrency 1..16, reconcile interval 5..3600s, reconcile grace 30..86400s, merge worker interval 1..3600s, trash retention 3600..31536000s, git concurrency 1..256, git organization floor 1..=git concurrency and ceiling >= floor (clamped to capacity at run time), git wait 1..60000ms, git timeout 10..86400s, git request limit 1024..1073741824 bytes, git push limit 1024..10737418240 bytes, repository quota bytes 0 or 1MiB..1TiB (not above an organization quota), organization quota bytes 0 or 1MiB..1TiB, API rates 1..1000000/min, Git rates 1..1000000/min, anonymous fetch 1..1000000 per address and 1..10000000 globally per minute, anonymous login 1..1000000 per address and 1..10000000 globally per minute, organization creation 1..1000000 per account/installation, job organization share 1..64 (clamped to the worker count), email 1..1000000/h, signup velocity 1..1000000 per 1..86400s, DKIM selector non-whitespace and at most 255 bytes, object upload deadline 60..2592000s, job concurrency 1..64, job timeout 1..3600s, job poll 50..60000ms, job retention 1..365d, outbox poll 50..60000ms, login throttle 1..1000000 failed sign-ins per username and per client inside a 1..86400s window, login throttle delay base 1..60000ms and max 1..600000ms (max >= base), backup directory must not be empty, maintenance drain timeout 1..86400s, housekeeping interval 60..2592000s, housekeeping grace 0..31536000s, housekeeping concurrency 1..16, housekeeping timeout 10..86400s, housekeeping live-set cap 1MiB..1GiB, housekeeping pin retention 1..3650d, housekeeping fork fetch limit 1MiB..1GiB, housekeeping job budget (7*timeout+60s) <= 604860s"
            );
        }
        if self.storage_root.as_os_str().is_empty() {
            bail!("storage_root must not be empty");
        }
        if !matches!(self.installation_key_method.as_str(), "file" | "kms") {
            bail!("installation_key_method must be \"file\" or \"kms\"");
        }
        if !matches!(self.backup_snapshot_mode.as_str(), "auto" | "fence") {
            bail!("backup_snapshot_mode must be \"auto\" or \"fence\"");
        }
        if !self.installation_key_previous_file.as_os_str().is_empty()
            && self.installation_key_file.as_os_str().is_empty()
        {
            bail!("installation_key_previous_file requires installation_key_file");
        }
        if !self.installation_key_file.as_os_str().is_empty()
            && self.installation_key_file == self.installation_key_previous_file
        {
            bail!(
                "installation_key_previous_file must name a different file from \
                 installation_key_file"
            );
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
        self.check_private_paths_outside_storage()?;
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
        if !(1..=300).contains(&self.outbound_timeout_seconds) {
            bail!("outbound_timeout_seconds must be between 1 and 300");
        }
        if !(1024..=67_108_864).contains(&self.outbound_max_response_bytes) {
            bail!("outbound_max_response_bytes must be between 1024 and 67108864 bytes");
        }
        if !(1024..=16_777_216).contains(&self.outbound_max_request_bytes) {
            bail!("outbound_max_request_bytes must be between 1024 and 16777216 bytes");
        }
        if self.outbound_max_redirects > 10 {
            bail!("outbound_max_redirects must be between 0 and 10");
        }
        for entry in &self.outbound_allowlist {
            crate::outbound::validate_allowlist_entry(entry)
                .map_err(|reason| anyhow::anyhow!("outbound_allowlist: {reason}"))?;
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
            if !self.smtp_bounce_address.trim().is_empty()
                && !crate::invitations::valid_email(&self.smtp_bounce_address)
            {
                bail!("smtp_bounce_address must be a valid email address when set");
            }
            if !self.abuse_contact.trim().is_empty()
                && !crate::invitations::valid_email(&self.abuse_contact)
            {
                bail!("abuse_contact must be a valid email address when set");
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

    /// The operator-custody directory for per-account and per-organization
    /// data keys. An explicit `data_key_directory` wins; otherwise it is a
    /// `data-keys` directory beside `installation_key_file`. It is never inside
    /// the database or an archive, which is what makes deleting a key shred the
    /// secrets it protected in older backups.
    pub fn data_key_directory(&self) -> PathBuf {
        if !self.data_key_directory.as_os_str().is_empty() {
            return self.data_key_directory.clone();
        }
        if self.installation_key_file.as_os_str().is_empty() {
            return PathBuf::new();
        }
        self.installation_key_file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("data-keys")
    }

    /// The storage tar carries `storage_root` verbatim, so an installation key
    /// or another private operator file inside it would travel in every backup.
    /// This is part of `validate` (not only `backup`) so `serve`, `migrate` and
    /// `doctor` refuse at start rather than discovering it when a backup runs.
    pub(crate) fn check_private_paths_outside_storage(&self) -> Result<()> {
        // `canonicalize` fails for a path that does not exist yet (a key created
        // after setup) and resolves `/tmp` to `/private/tmp` on macOS. Resolve
        // the parent in that case so the comparison is still real.
        fn resolve(path: &Path) -> PathBuf {
            std::fs::canonicalize(path).unwrap_or_else(|_| {
                match (path.parent(), path.file_name()) {
                    (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                        .map(|parent| parent.join(name))
                        .unwrap_or_else(|_| path.to_path_buf()),
                    _ => path.to_path_buf(),
                }
            })
        }
        let storage = resolve(&self.storage_root);
        let mut paths: Vec<(&str, &Path)> = Vec::new();
        if !self.installation_key_file.as_os_str().is_empty() {
            paths.push(("installation key", &self.installation_key_file));
        }
        if !self.installation_key_previous_file.as_os_str().is_empty() {
            paths.push((
                "previous installation key",
                &self.installation_key_previous_file,
            ));
        }
        if let Some(path) = &self.installation_key_passphrase_file {
            paths.push(("installation key passphrase", path));
        }
        let data_key_directory = self.data_key_directory();
        if !data_key_directory.as_os_str().is_empty() {
            paths.push(("data key directory", &data_key_directory));
        }
        if let Some(path) = &self.smtp_password_file {
            paths.push(("SMTP credential", path));
        }
        if !self.backup_key_file.as_os_str().is_empty() {
            paths.push(("backup key", &self.backup_key_file));
        }
        for (label, path) in paths {
            if path.as_os_str().is_empty() {
                continue;
            }
            let resolved = resolve(path);
            if resolved.starts_with(&storage) {
                bail!(
                    "the {label} {} is inside storage_root {}; move it outside the storage root \
                     before starting the server or taking a backup, or the storage tar would \
                     carry it",
                    resolved.display(),
                    storage.display()
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
    fn object_store_limits_default_to_the_published_quotas_and_are_bounded() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        assert_eq!(c.repository_quota_bytes, 5 * 1024 * 1024 * 1024);
        assert_eq!(c.organization_quota_bytes, 50 * 1024 * 1024 * 1024);
        assert_eq!(c.object_upload_deadline_seconds, 86_400, "one day");
        assert!(c.validate().is_ok());
        c.organization_quota_bytes = 1;
        assert!(c.validate().is_err(), "a sub-1MiB quota is refused");
        c.repository_quota_bytes = 0;
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
    fn fairness_rate_limits_are_bounded() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        assert!(c.validate().is_ok(), "the defaults fit every bound");
        for bad in [0, 1_000_001] {
            c.git_requests_per_organization = bad;
            assert!(
                c.validate().is_err(),
                "git organization rate {bad} must be refused"
            );
        }
        c.git_requests_per_organization = 600;
        for bad in [0, 1_000_001] {
            c.git_requests_per_account = bad;
            assert!(
                c.validate().is_err(),
                "git account rate {bad} must be refused"
            );
        }
        c.git_requests_per_account = 120;
        for bad in [0, 10_000_001] {
            c.anonymous_login_global = bad;
            assert!(
                c.validate().is_err(),
                "anonymous login global rate {bad} must be refused"
            );
        }
        c.anonymous_login_global = 300;
        assert!(c.validate().is_ok(), "the largest valid rates are accepted");
        c.git_requests_per_account = 0;
        let error = c.validate().unwrap_err().to_string();
        assert!(error.contains("Git rates 1..1000000/min"), "{error}");
        assert!(
            error.contains("anonymous login 1..1000000 per address and 1..10000000 globally"),
            "{error}"
        );
    }

    #[test]
    fn outbound_client_limits_and_allowlist_are_validated() {
        let mut c = Config {
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        };
        // The published defaults: nothing is callable until an operator names
        // it; 30 s, 5 MiB response, 1 MiB request, 5 redirects.
        assert!(c.outbound_allowlist.is_empty());
        assert!(!c.outbound_allow_private);
        assert!(!c.outbound_allow_metadata);
        assert_eq!(c.outbound_timeout_seconds, 30);
        assert_eq!(c.outbound_max_response_bytes, 5_242_880);
        assert_eq!(c.outbound_max_request_bytes, 1_048_576);
        assert_eq!(c.outbound_max_redirects, 5);
        assert!(c.validate().is_ok(), "an empty allowlist is a valid config");

        for bad in [0, 301] {
            c.outbound_timeout_seconds = bad;
            assert!(c.validate().is_err(), "timeout {bad} must be refused");
        }
        c.outbound_timeout_seconds = 30;
        for bad in [0, 1023, 67_108_865] {
            c.outbound_max_response_bytes = bad;
            assert!(
                c.validate().is_err(),
                "response bound {bad} must be refused"
            );
        }
        c.outbound_max_response_bytes = 5_242_880;
        c.outbound_max_request_bytes = 1023;
        assert!(c.validate().is_err());
        c.outbound_max_request_bytes = 1_048_576;
        c.outbound_max_redirects = 11;
        assert!(c.validate().is_err());
        c.outbound_max_redirects = 0;
        assert!(c.validate().is_ok(), "zero redirects is a valid policy");

        c.outbound_allowlist = vec!["api.example.com".into(), "*.hooks.example.com".into()];
        assert!(c.validate().is_ok());
        for bad in [
            "https://api.example.com".to_owned(),
            "api.example.com/path".to_owned(),
            "api example.com".to_owned(),
            "*.*.example.com".to_owned(),
        ] {
            c.outbound_allowlist = vec![bad.clone()];
            assert!(c.validate().is_err(), "{bad} must be refused");
        }
    }

    /// The shipped example's outbound block, uncommented as written, must load.
    #[test]
    fn the_example_file_suggests_only_outbound_values_the_validator_accepts() {
        const EXAMPLE: &str = include_str!("../../../githearth.example.toml");
        let outbound: Vec<&str> = EXAMPLE
            .lines()
            .filter_map(|line| line.trim().strip_prefix('#'))
            .map(str::trim)
            .filter(|line| line.starts_with("outbound_"))
            .collect();
        assert!(
            outbound
                .iter()
                .any(|line| line.starts_with("outbound_allowlist = ")),
            "the example no longer shows the allowlist: {outbound:?}"
        );
        let toml = outbound.join("\n");
        let mut config: Config = toml::from_str(&toml)
            .unwrap_or_else(|e| panic!("the example's outbound settings do not parse: {e}"));
        config.database_url = "postgres://localhost/test".into();
        assert!(
            config.validate().is_ok(),
            "uncommenting the example's outbound block does not load"
        );
        assert!(!config.outbound_allow_private);
        assert!(!config.outbound_allow_metadata);
        assert_eq!(config.outbound_timeout_seconds, 30);
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
