//! Per-organization and per-account limits (M4.2, case U30).
//!
//! The defaults are the published values in ADR 0014 §5 and
//! `docs/operations/limits.md`. Every budget here is an in-process fixed
//! window: it is deliberately cheap, needs no database write on a hot path and
//! cannot turn a database outage into an unhandled request. The windows tick
//! from the first request in each window; a caller that finishes a window does
//! not keep another client waiting.
//!
//! The limiter is used at the interface, not only in a configuration table:
//!
//! * sign-in calls [`Limits::allow_anonymous_login`] before hashing a password,
//!   for every login path (JSON, browser and Git basic);
//! * the JSON API charges [`Limits::allow_api`] in one middleware layer, so
//!   every `/api/v1` route — not only the two creation endpoints — spends the
//!   published organization and account budgets;
//! * authenticated Git HTTP and SSH call [`Limits::allow_git`] after admission,
//!   separately from the API budgets;
//! * anonymous Git fetch calls [`Limits::allow_anonymous_fetch`] after the
//!   repository is known to exist and before spawning `upload-pack`;
//! * organization creation calls [`Limits::allow_organization_creation`];
//! * the notification and verification consumers call [`Limits::allow_email`];
//! * M11a's sign-up endpoint will call [`Limits::allow_signup`] (the limiter is
//!   tested here at its interface, as U30b requires).

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::config::Config;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3_600);
const DAY: Duration = Duration::from_secs(86_400);

/// A fixed-window counter. One mutex per budget, never held across an await.
struct Window {
    limit: u64,
    window: Duration,
    entries: Mutex<HashMap<String, (Instant, u64)>>,
}

impl Window {
    fn new(limit: u64, window: Duration) -> Self {
        Self {
            limit: limit.max(1),
            window,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn allow_at(&self, key: &str, now: Instant) -> bool {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit {
            false
        } else {
            entry.1 += 1;
            true
        }
    }

    /// Drop expired keys so a long-lived process does not remember every
    /// address that ever made one request. Called opportunistically; a missed
    /// sweep only costs memory until the next call.
    fn sweep_at(&self, now: Instant) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, (started, _)| now.duration_since(*started) < self.window);
    }
}

/// The installation's live limit budgets, built once from configuration.
pub struct Limits {
    api_organization: Window,
    api_account: Window,
    git_organization: Window,
    git_account: Window,
    anonymous_fetch_address: Window,
    anonymous_fetch_global: Window,
    anonymous_login_address: Window,
    anonymous_login_global: Window,
    organization_creation_account: Window,
    organization_creation_installation: Window,
    email_organization: Window,
    email_security_address: Window,
    signup_address: Window,
}

impl Limits {
    pub fn from_config(config: &Config) -> Self {
        Self {
            api_organization: Window::new(config.api_requests_per_organization, MINUTE),
            api_account: Window::new(config.api_requests_per_account, MINUTE),
            git_organization: Window::new(config.git_requests_per_organization, MINUTE),
            git_account: Window::new(config.git_requests_per_account, MINUTE),
            anonymous_fetch_address: Window::new(config.anonymous_fetch_per_address, MINUTE),
            anonymous_fetch_global: Window::new(config.anonymous_fetch_global, MINUTE),
            anonymous_login_address: Window::new(config.anonymous_login_per_address, MINUTE),
            anonymous_login_global: Window::new(config.anonymous_login_global, MINUTE),
            organization_creation_account: Window::new(
                config.organization_creation_per_account,
                DAY,
            ),
            organization_creation_installation: Window::new(
                config.organization_creation_per_installation,
                DAY,
            ),
            email_organization: Window::new(config.email_per_organization, HOUR),
            email_security_address: Window::new(config.email_security_per_address, HOUR),
            signup_address: Window::new(
                config.signup_velocity_per_address,
                Duration::from_secs(config.signup_velocity_window_seconds),
            ),
        }
    }

    /// Per-organization API budget. `false` means the request is over its
    /// share and the caller answers `429` (or Git's `Retry-After`).
    pub fn allow_api_organization(&self, organization: Uuid) -> bool {
        self.allow_api_organization_at(organization, Instant::now())
    }

    pub fn allow_api_organization_at(&self, organization: Uuid, now: Instant) -> bool {
        self.api_organization
            .allow_at(&organization.to_string(), now)
    }

    /// Per-account API budget.
    pub fn allow_api_account(&self, account: Uuid) -> bool {
        self.allow_api_account_at(account, Instant::now())
    }

    pub fn allow_api_account_at(&self, account: Uuid, now: Instant) -> bool {
        self.api_account.allow_at(&account.to_string(), now)
    }

    /// One authenticated API request: the organization budget and, when the
    /// caller is signed in, the account budget. The JSON API charges this in
    /// one middleware layer, so a new route cannot forget it.
    pub fn allow_api(&self, organization: Option<Uuid>, account: Option<Uuid>) -> bool {
        self.allow_api_at(organization, account, Instant::now())
    }

    pub fn allow_api_at(
        &self,
        organization: Option<Uuid>,
        account: Option<Uuid>,
        now: Instant,
    ) -> bool {
        if let Some(organization) = organization
            && !self.allow_api_organization_at(organization, now)
        {
            return false;
        }
        match account {
            Some(account) => self.allow_api_account_at(account, now),
            None => true,
        }
    }

    /// One authenticated Git transport request. Both the organization and the
    /// account are charged, separately from the API budgets (ADR 0014 §5).
    pub fn allow_git(&self, organization: Uuid, account: Option<Uuid>) -> bool {
        self.allow_git_at(organization, account, Instant::now())
    }

    pub fn allow_git_at(&self, organization: Uuid, account: Option<Uuid>, now: Instant) -> bool {
        if !self
            .git_organization
            .allow_at(&organization.to_string(), now)
        {
            return false;
        }
        match account {
            Some(account) => self.git_account.allow_at(&account.to_string(), now),
            None => true,
        }
    }

    /// Anonymous clone/fetch. The installation budget is checked too, so a
    /// client that changes address cannot exhaust the installation's public
    /// capacity. An unresolved address is counted under one shared key rather
    /// than bypassing the per-address budget.
    pub fn allow_anonymous_fetch(&self, address: Option<IpAddr>) -> bool {
        self.allow_anonymous_fetch_at(address, Instant::now())
    }

    pub fn allow_anonymous_fetch_at(&self, address: Option<IpAddr>, now: Instant) -> bool {
        let key = address
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unresolved".to_owned());
        // The address budget is charged first: an address already over its
        // share must not consume the installation's remaining capacity.
        if !self.anonymous_fetch_address.allow_at(&key, now) {
            return false;
        }
        self.anonymous_fetch_global.allow_at("installation", now)
    }

    /// Anonymous login: per client address and across the installation. Sign-in
    /// has a separate failure throttle; this is the request-rate ceiling that
    /// keeps a public login form from being used as a password-hashing
    /// amplifier. The installation budget is what a second address cannot
    /// escape. No search endpoint exists in this build (M9 owns it); a search
    /// surface added later must charge this same counter.
    pub fn allow_anonymous_login(&self, address: Option<IpAddr>) -> bool {
        self.allow_anonymous_login_at(address, Instant::now())
    }

    pub fn allow_anonymous_login_at(&self, address: Option<IpAddr>, now: Instant) -> bool {
        let key = address
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unresolved".to_owned());
        if !self.anonymous_login_address.allow_at(&key, now) {
            return false;
        }
        self.anonymous_login_global.allow_at("installation", now)
    }

    /// Organization creation: both the account and the installation caps.
    pub fn allow_organization_creation(&self, account: Uuid) -> bool {
        self.allow_organization_creation_at(account, Instant::now())
    }

    pub fn allow_organization_creation_at(&self, account: Uuid, now: Instant) -> bool {
        if !self
            .organization_creation_account
            .allow_at(&account.to_string(), now)
        {
            return false;
        }
        self.organization_creation_installation
            .allow_at("installation", now)
    }

    /// Email budgets. Security mail is bounded per destination address;
    /// notification mail is bounded per organization. Both share one hour.
    pub fn allow_email(&self, organization: Option<Uuid>, to: &str, security: bool) -> bool {
        self.allow_email_at(organization, to, security, Instant::now())
    }

    pub fn allow_email_at(
        &self,
        organization: Option<Uuid>,
        to: &str,
        security: bool,
        now: Instant,
    ) -> bool {
        let address = to.trim().to_ascii_lowercase();
        if security {
            return self.email_security_address.allow_at(&address, now);
        }
        match organization {
            Some(org) => self.email_organization.allow_at(&org.to_string(), now),
            None => self.email_organization.allow_at("installation", now),
        }
    }

    /// Sign-up velocity, per address. M11a's sign-up endpoint calls this; the
    /// endpoint is not part of M4.2, but the limiter is tested here.
    pub fn allow_signup(&self, address: Option<IpAddr>) -> bool {
        self.allow_signup_at(address, Instant::now())
    }

    pub fn allow_signup_at(&self, address: Option<IpAddr>, now: Instant) -> bool {
        let key = address
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unresolved".to_owned());
        self.signup_address.allow_at(&key, now)
    }

    /// How many keys the fixed windows currently remember. Observability for
    /// the sweep and for a future gauge; it names no client.
    pub fn tracked_keys(&self) -> usize {
        [
            &self.api_organization,
            &self.api_account,
            &self.git_organization,
            &self.git_account,
            &self.anonymous_fetch_address,
            &self.anonymous_fetch_global,
            &self.anonymous_login_address,
            &self.anonymous_login_global,
            &self.organization_creation_account,
            &self.organization_creation_installation,
            &self.email_organization,
            &self.email_security_address,
            &self.signup_address,
        ]
        .into_iter()
        .map(|window| {
            window
                .entries
                .lock()
                .map(|entries| entries.len())
                .unwrap_or(0)
        })
        .sum()
    }

    /// Periodic housekeeping for the in-memory maps. The runner calls this
    /// from its existing hourly sweep; a missed call costs memory, never a
    /// wrong decision.
    pub fn sweep(&self) {
        let now = Instant::now();
        for window in [
            &self.api_organization,
            &self.api_account,
            &self.git_organization,
            &self.git_account,
            &self.anonymous_fetch_address,
            &self.anonymous_fetch_global,
            &self.anonymous_login_address,
            &self.anonymous_login_global,
            &self.organization_creation_account,
            &self.organization_creation_installation,
            &self.email_organization,
            &self.email_security_address,
            &self.signup_address,
        ] {
            window.sweep_at(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::from_config(&Config::default())
    }

    #[test]
    fn an_organization_budget_is_per_organization_and_resets_with_the_window() {
        let limits = limits();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let start = Instant::now();
        for i in 0..300 {
            assert!(
                limits.allow_api_organization_at(a, start + Duration::from_millis(i)),
                "request {i} inside the organization budget"
            );
        }
        assert!(
            !limits.allow_api_organization_at(a, start + Duration::from_secs(1)),
            "the 301st request is refused"
        );
        assert!(
            limits.allow_api_organization_at(b, start + Duration::from_secs(1)),
            "another organization has its own budget"
        );
        assert!(
            limits.allow_api_organization_at(a, start + MINUTE),
            "the fixed window has expired"
        );
    }

    #[test]
    fn an_account_budget_is_sixty_per_minute() {
        let limits = limits();
        let account = Uuid::new_v4();
        let start = Instant::now();
        for _ in 0..60 {
            assert!(limits.allow_api_account_at(account, start));
        }
        assert!(!limits.allow_api_account_at(account, start + Duration::from_secs(1)));
        assert!(
            limits.allow_api_account_at(account, start + MINUTE),
            "the next window opens"
        );
    }

    #[test]
    fn anonymous_fetch_is_per_address_and_globally() {
        let limits = limits();
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "203.0.113.2".parse().unwrap();
        let start = Instant::now();
        for i in 0..120 {
            assert!(limits.allow_anonymous_fetch_at(Some(a), start + Duration::from_millis(i)));
        }
        assert!(
            !limits.allow_anonymous_fetch_at(Some(a), start + Duration::from_secs(1)),
            "the address budget is spent"
        );
        assert!(
            limits.allow_anonymous_fetch_at(Some(b), start + Duration::from_secs(1)),
            "another address is unaffected"
        );
        // Spend the global budget with distinct addresses. Each address can
        // contribute at most 120, so this exercises the global counter.
        let mut allowed = 0;
        for i in 0..20u32 {
            let ip: IpAddr = format!("198.51.100.{i}").parse().unwrap();
            for _ in 0..120 {
                if limits.allow_anonymous_fetch_at(Some(ip), start + Duration::from_secs(2)) {
                    allowed += 1;
                }
            }
        }
        assert_eq!(
            allowed,
            2_000 - 121,
            "the installation budget bounds the rest"
        );
        assert!(!limits.allow_anonymous_fetch_at(Some(b), start + Duration::from_secs(3)));
        assert!(
            limits.allow_anonymous_fetch_at(Some(b), start + MINUTE),
            "the minute window resets"
        );
    }

    #[test]
    fn anonymous_login_is_twenty_per_address_per_minute() {
        let limits = limits();
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let start = Instant::now();
        for _ in 0..20 {
            assert!(limits.allow_anonymous_login_at(Some(ip), start));
        }
        assert!(!limits.allow_anonymous_login_at(Some(ip), start));
        assert!(
            limits.allow_anonymous_login_at(Some(ip), start + MINUTE),
            "the login window resets"
        );
    }

    #[test]
    fn anonymous_login_has_an_installation_budget_a_second_address_cannot_escape() {
        let config = Config {
            anonymous_login_per_address: 2,
            anonymous_login_global: 3,
            ..Config::default()
        };
        let limits = Limits::from_config(&config);
        let a: IpAddr = "203.0.113.30".parse().unwrap();
        let b: IpAddr = "203.0.113.31".parse().unwrap();
        let start = Instant::now();
        assert!(limits.allow_anonymous_login_at(Some(a), start));
        assert!(limits.allow_anonymous_login_at(Some(a), start));
        assert!(!limits.allow_anonymous_login_at(Some(a), start));
        // The third installation slot is spent by the second address, and the
        // fourth is refused even though that address is under its own budget.
        assert!(limits.allow_anonymous_login_at(Some(b), start));
        assert!(
            !limits.allow_anonymous_login_at(Some(b), start),
            "the installation budget bounds a second address"
        );
        assert!(
            limits.allow_anonymous_login_at(Some(b), start + MINUTE),
            "the installation window resets"
        );
    }

    #[test]
    fn git_requests_are_charged_per_organization_and_per_account() {
        let config = Config {
            git_requests_per_organization: 3,
            git_requests_per_account: 1,
            ..Config::default()
        };
        let limits = Limits::from_config(&config);
        let organization = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let start = Instant::now();
        assert!(limits.allow_git_at(organization, Some(first), start));
        assert!(
            !limits.allow_git_at(organization, Some(first), start),
            "the account budget is one per minute"
        );
        assert!(
            limits.allow_git_at(organization, Some(second), start),
            "another account may spend the organization's third slot"
        );
        assert!(
            !limits.allow_git_at(organization, Some(second), start),
            "the organization budget is three per minute"
        );
        assert!(
            limits.allow_git_at(Uuid::new_v4(), Some(third), start),
            "another organization has its own budget"
        );
    }

    #[test]
    fn the_sweep_drops_expired_keys() {
        let limits = limits();
        let stale = Instant::now() - Duration::from_secs(120);
        assert!(limits.allow_api_organization_at(Uuid::new_v4(), stale));
        assert!(limits.allow_anonymous_login_at(Some("203.0.113.40".parse().unwrap()), stale));
        assert_eq!(limits.api_organization.entries.lock().unwrap().len(), 1);
        limits.sweep();
        assert_eq!(
            limits.api_organization.entries.lock().unwrap().len(),
            0,
            "the hourly sweep forgets expired keys"
        );
        assert_eq!(
            limits.anonymous_login_address.entries.lock().unwrap().len(),
            0
        );
    }

    #[test]
    fn organization_creation_is_capped_per_account_and_per_installation() {
        let config = Config {
            organization_creation_per_installation: 4,
            ..Config::default()
        };
        let limits = Limits::from_config(&config);
        let start = Instant::now();
        let first = Uuid::new_v4();
        for _ in 0..3 {
            assert!(limits.allow_organization_creation_at(first, start));
        }
        assert!(
            !limits.allow_organization_creation_at(first, start),
            "the account cap is three per day"
        );
        let second = Uuid::new_v4();
        assert!(
            limits.allow_organization_creation_at(second, start),
            "another account may use the installation's remaining allowance"
        );
        assert!(
            !limits.allow_organization_creation_at(Uuid::new_v4(), start),
            "the installation cap is four per day"
        );
        assert!(
            limits.allow_organization_creation_at(first, start + DAY),
            "the next day opens a fresh window"
        );
    }

    #[test]
    fn email_is_bounded_per_organization_and_per_security_address() {
        let limits = limits();
        let org = Uuid::new_v4();
        let start = Instant::now();
        for _ in 0..200 {
            assert!(limits.allow_email_at(Some(org), "a@example.com", false, start));
        }
        assert!(!limits.allow_email_at(Some(org), "a@example.com", false, start));
        assert!(
            limits.allow_email_at(Some(Uuid::new_v4()), "a@example.com", false, start),
            "another organization has its own share"
        );
        for _ in 0..5 {
            assert!(limits.allow_email_at(None, "b@example.com", true, start));
        }
        assert!(
            !limits.allow_email_at(None, "b@example.com", true, start),
            "security mail is five per address per hour"
        );
        assert!(
            limits.allow_email_at(None, "B@EXAMPLE.COM", true, start + HOUR),
            "the address key is case-insensitive and the hour resets"
        );
    }

    #[test]
    fn signup_velocity_is_per_address() {
        let limits = limits();
        let ip: IpAddr = "203.0.113.20".parse().unwrap();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limits.allow_signup_at(Some(ip), start));
        }
        assert!(!limits.allow_signup_at(Some(ip), start));
        assert!(
            limits.allow_signup_at(Some(ip), start + HOUR),
            "the velocity window resets after an hour"
        );
    }

    #[test]
    fn an_unresolved_address_is_still_bounded() {
        let limits = limits();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limits.allow_signup_at(None, start));
        }
        assert!(
            !limits.allow_signup_at(None, start),
            "a request without a resolved address cannot bypass the limiter"
        );
    }
}
