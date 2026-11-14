//! Bounded failed sign-ins (M4.1, acceptance case U03).
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
//! is not recorded, so throttling can never extend itself.
//!
//! Rows hold a SHA-256 of the normalized username and the resolved address,
//! never a password, and are pruned once they are a day old (beyond every
//! allowed window).

use std::net::IpAddr;

use sha2::{Digest, Sha256};

use crate::{ApiError, CLIENT_IP};

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

/// Whether this username or client address has used up its failed attempts.
/// A refusal is the same whether or not the username exists.
pub(crate) async fn throttled(
    pool: &sqlx::PgPool,
    config: &crate::config::Config,
    request: Request,
    username: &str,
) -> Result<bool, ApiError> {
    let window = config.login_throttle_window_seconds as f64;
    let per_username = config.login_throttle_max_per_username.min(i64::MAX as u64) as i64;
    let per_address = config.login_throttle_max_per_client.min(i64::MAX as u64) as i64;
    let address = match request.client_ip() {
        Some(ip) => Some(subject_address(pool, ip).await?),
        None => None,
    };
    // One statement, so both bounds read the same clock and the same window.
    // A NULL address matches nothing, so the address bound never counts an
    // unidentifiable caller.
    let blocked: bool = sqlx::query_scalar(
        "SELECT EXISTS (
   SELECT 1 FROM login_attempts WHERE scope = 'username' AND subject = $1
     AND attempted_at > clock_timestamp() - $4 * interval '1 second'
   HAVING count(*) >= $3
 ) OR EXISTS (
   SELECT 1 FROM login_attempts WHERE scope = 'client_ip' AND subject = $2
     AND attempted_at > clock_timestamp() - $4 * interval '1 second'
   HAVING count(*) >= $5
 )",
    )
    .bind(subject_username(username))
    .bind(address)
    .bind(per_username)
    .bind(window)
    .bind(per_address)
    .fetch_one(pool)
    .await?;
    Ok(blocked)
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
}
