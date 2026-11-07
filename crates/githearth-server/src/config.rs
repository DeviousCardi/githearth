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
        }
    }
}
fn default_secure_cookies() -> bool {
    true
}
fn default_ssh_bind() -> SocketAddr {
    "127.0.0.1:2222".parse().unwrap()
}

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
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
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
        {
            bail!(
                "configuration bounds: pool 1..100, DB timeout 1..30s, request timeout 1..120s, token TTL 1..86400s, body 1024..1048576 bytes, auth concurrency 1..16, reconcile interval 5..3600s, reconcile grace 30..86400s, merge worker interval 1..3600s, trash retention 3600..31536000s, git concurrency 1..256, git timeout 10..86400s, git request limit 1024..1073741824 bytes, git push limit 1024..10737418240 bytes, repository quota bytes 0 or 1MiB..1TiB"
            );
        }
        if self.storage_root.as_os_str().is_empty() {
            bail!("storage_root must not be empty");
        }
        if self.ssh_enabled && self.ssh_host_key_path.as_os_str().is_empty() {
            bail!("ssh_host_key_path must be set when ssh_enabled is true");
        }
        Ok(())
    }
}

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
