//! Deterministic hold points used by integration tests.
//!
//! A seam does nothing unless the environment variable
//! `GITHEARTH_TEST_HOLD_<NAME>` names a marker path. When it does, the seam
//! writes the marker and waits (bounded) for `<marker>.release`, so a test can
//! observe a unit of work *inside* its write permit — or between the dump and
//! the snapshot — and release it at the exact moment it wants.
//!
//! These are not a production feature: an installation with no such variable
//! set executes the seam as an empty call. They exist so the tests can force a
//! lifetime rather than race a sleep.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// The longest a held seam waits before proceeding anyway, so a broken test
/// cannot wedge a server forever.
const HOLD_LIMIT: Duration = Duration::from_secs(60);

pub(crate) async fn hold(name: &str) {
    let Ok(marker) = std::env::var(format!("GITHEARTH_TEST_HOLD_{name}")) else {
        return;
    };
    if marker.is_empty() {
        return;
    }
    let marker = PathBuf::from(marker);
    let release = marker.with_extension("release");
    let _ = fs::write(&marker, b"held");
    let deadline = Instant::now() + HOLD_LIMIT;
    while !release.exists() {
        if Instant::now() >= deadline {
            tracing::warn!(seam = name, "test seam hold timed out; continuing");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
