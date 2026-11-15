//! Bounded failed sign-ins (M4.1, acceptance cases U03 and U18).
//!
//! Each failed password check is recorded twice: once against the attempted
//! username and once against the resolved client address (the same
//! `client_ip.rs` resolution the audit log uses, so a trusted proxy's
//! `X-Forwarded-For` is honoured and an untrusted one's is ignored). A sign-in
//! is refused before the password is hashed once either counter has reached its
//! configured limit.
//!
//! The bounds are sliding windows stored in PostgreSQL, so they need no
//! process memory and are shared by every instance talking to the database.
//! Nothing is ever locked: once an attempt leaves the window it stops
//! counting, and a successful sign-in clears its username's counter, so an
//! administrator can always wait out (or reset) a throttle. A refused sign-in
//! is not recorded, so throttling can never extend itself — in particular, a
//! client cannot keep an account throttled by continuing to knock.
//!
//! After the fifth recorded failure inside the window, every password attempt
//! also waits. The wait is a function of the failure count alone —
//! `base * 2^(failures - 5)` milliseconds, capped at the configured maximum —
//! taking the larger of the account's and the address's counts, so it grows
//! when an attacker sprays many accounts from one address. It is charged
//! before the password is hashed and before an authentication slot is taken,
//! and it applies to a correct password too: inside the window the attempt is
//! refused as before, only more slowly. It never locks anyone out permanently:
//! the count that drives it ages out with the window, a successful sign-in
//! clears the account's count, and `reset-admin` clears it as well. Token and
//! SSH-key authentication never pass through this module, so they are exempt.
//!
//! Rows hold a SHA-256 of the normalized username and the resolved address,
//! never a password, and are pruned once they are a day old (beyond every
//! allowed window).

use std::net::IpAddr;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::{ApiError, CLIENT_IP};

/// The failure count at which the exponential delay begins (U18).
pub(crate) const DELAY_AFTER_FAILURES: i64 = 5;

/// The address a sign-in attempt came from, resolved once per request by the
/// client-IP middleware and re-used by every sign-in path in that request.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Request {
    client_ip: Option<IpAddr>,
}

impl Request {
    /// The address already resolved for the current request, when there is one.
    /// Every HTTP route, including Git over HTTP, runs inside the client-IP
    /// middleware's scope, so this is the same address the audit log records.
    pub(crate) fn from_current() -> Self {
        Self {
            client_ip: CLIENT_IP.try_with(|ip| *ip).ok(),
        }
    }

    /// Built directly from an address (unit tests, and callers without the
    /// middleware).
    #[cfg(test)]
    pub(crate) fn at(client_ip: IpAddr) -> Self {
        Self {
            client_ip: Some(client_ip),
        }
    }

    pub(crate) fn client_ip(self) -> Option<IpAddr> {
        self.client_ip
    }
}

/// A username normalized for counting: lower case and trimmed, then hashed so
/// the table does not store the attempted spelling. Unambiguous because the
/// normalized value is matched exactly, never as a SQL pattern.
pub fn subject_username(username: &str) -> String {
    let normalized = username.trim().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

async fn subject_address(pool: &sqlx::PgPool, ip: IpAddr) -> Result<String, sqlx::Error> {
    // PostgreSQL's `host()` gives the canonical text for both families.
    sqlx::query_scalar("SELECT host($1::inet)")
        .bind(ip.to_string())
        .fetch_one(pool)
        .await
}

/// The decision for one password attempt: whether the bound refuses it and how
/// long it must wait first.
pub(crate) struct Attempt {
    pub(crate) blocked: bool,
    pub(crate) delay: Duration,
}

/// The exponential wait after the fifth recorded failure (U18).
///
/// It is a function of the failure count alone, not of how long the window has
/// left: `base * 2^(failures - 5)` milliseconds, capped at
/// `login_throttle_delay_max_ms`. The window matters only by making failures
/// expire, after which the delay is zero again. A cap that is not a power of
/// the base still bounds the wait; the delay never grows without limit.
pub fn retry_delay(config: &crate::config::Config, failures: i64) -> Duration {
    if failures < DELAY_AFTER_FAILURES {
        return Duration::ZERO;
    }
    // The cap is at most 600 s, so 20 doublings always reach it; clamping the
    // exponent also keeps the shift in range.
    let exponent = (failures - DELAY_AFTER_FAILURES).clamp(0, 20) as u32;
    let millis = config
        .login_throttle_delay_base_ms
        .saturating_mul(1u64 << exponent);
    Duration::from_millis(millis.min(config.login_throttle_delay_max_ms))
}

/// Reads the two counters for this attempt and decides both its refusal and
/// its delay. One statement, so the account and address bounds read the same
/// clock and the same window; a NULL address counts zero, so the address bound
/// never counts an unidentifiable caller.
pub(crate) async fn examine(
    pool: &sqlx::PgPool,
    config: &crate::config::Config,
    request: Request,
    username: &str,
) -> Result<Attempt, ApiError> {
    let window = config.login_throttle_window_seconds as f64;
    let per_username = config.login_throttle_max_per_username.min(i64::MAX as u64) as i64;
    let per_address = config.login_throttle_max_per_client.min(i64::MAX as u64) as i64;
    let address = match request.client_ip() {
        Some(ip) => Some(subject_address(pool, ip).await?),
        None => None,
    };
    let (username_failures, address_failures): (i64, i64) = sqlx::query_as(
        "SELECT
   (SELECT count(*) FROM login_attempts WHERE scope = 'username' AND subject = $1
      AND attempted_at > clock_timestamp() - $3 * interval '1 second'),
   (SELECT count(*) FROM login_attempts WHERE scope = 'client_ip' AND subject = $2
      AND attempted_at > clock_timestamp() - $3 * interval '1 second')",
    )
    .bind(subject_username(username))
    .bind(address)
    .bind(window)
    .fetch_one(pool)
    .await?;
    Ok(Attempt {
        blocked: username_failures >= per_username || address_failures >= per_address,
        delay: retry_delay(config, username_failures.max(address_failures)),
    })
}

/// Charges the delay, then refuses a used-up bound. Shared by the JSON login,
/// the browser form, Git HTTP basic authentication and the current-password
/// checks in re-authentication and password change, so all of them share one
/// bound and one wait.
pub(crate) async fn enforce(
    pool: &sqlx::PgPool,
    config: &crate::config::Config,
    request: Request,
    username: &str,
) -> Result<(), ApiError> {
    let attempt = examine(pool, config, request, username).await?;
    if !attempt.delay.is_zero() {
        tokio::time::sleep(attempt.delay).await;
    }
    if attempt.blocked {
        return Err(crate::too_many_attempts());
    }
    Ok(())
}

/// Records one failed sign-in against both its username and its address, and
/// prunes rows whose window has passed.
pub(crate) async fn failed(
    pool: &sqlx::PgPool,
    request: Request,
    username: &str,
) -> Result<(), ApiError> {
    let address = match request.client_ip() {
        Some(ip) => Some(subject_address(pool, ip).await?),
        None => None,
    };
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO login_attempts (scope, subject, attempted_at)
         SELECT 'username', $1, clock_timestamp()
         UNION ALL
         SELECT 'client_ip', $2::text, clock_timestamp() WHERE $2::text IS NOT NULL",
    )
    .bind(subject_username(username))
    .bind(address)
    .execute(&mut *tx)
    .await?;
    // Anything older than a day is outside every window the configuration
    // allows, so the table stays proportional to recent activity.
    sqlx::query(
        "DELETE FROM login_attempts WHERE attempted_at < clock_timestamp() - interval '1 day'",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A successful sign-in clears the username's counter, which is what keeps
/// the bound from ever becoming a permanent lockout. The address counter is
/// left alone: otherwise one valid account would reset it for an attacker.
pub(crate) async fn succeeded(pool: &sqlx::PgPool, username: &str) -> Result<(), ApiError> {
    sqlx::query("DELETE FROM login_attempts WHERE scope = 'username' AND subject = $1")
        .bind(subject_username(username))
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_normalized_before_counting() {
        assert_eq!(subject_username("Alice"), subject_username(" alice "));
        assert_eq!(subject_username("alice").len(), 64);
        assert_ne!(subject_username("alice"), subject_username("alicee"));
        // Hashed, so the table never holds the attempted spelling.
        assert!(!subject_username("alice").contains("alice"));
    }

    #[test]
    fn request_without_an_address_has_no_client_ip() {
        assert!(Request::default().client_ip().is_none());
        let ip = "203.0.113.9".parse().unwrap();
        assert_eq!(Request::at(ip).client_ip(), Some(ip));
    }

    #[test]
    fn the_delay_starts_after_the_fifth_failure_doubles_and_is_capped() {
        let c = crate::config::Config::default();
        for failures in 0..DELAY_AFTER_FAILURES {
            assert_eq!(
                retry_delay(&c, failures),
                Duration::ZERO,
                "failures {failures}"
            );
        }
        // Defaults: 200 ms after the fifth failure, doubling per failure, with
        // a 2 s cap.
        let expected = [
            (5, 200),
            (6, 400),
            (7, 800),
            (8, 1_600),
            (9, 2_000),
            (10, 2_000),
            (1_000, 2_000),
        ];
        for (failures, millis) in expected {
            assert_eq!(
                retry_delay(&c, failures),
                Duration::from_millis(millis),
                "failures {failures}"
            );
        }
        // A configured cap below one doubling step still bounds the wait.
        let mut small = c.clone();
        small.login_throttle_delay_base_ms = 10;
        small.login_throttle_delay_max_ms = 25;
        assert_eq!(retry_delay(&small, 5), Duration::from_millis(10));
        assert_eq!(retry_delay(&small, 6), Duration::from_millis(20));
        assert_eq!(retry_delay(&small, 7), Duration::from_millis(25));
    }
}
