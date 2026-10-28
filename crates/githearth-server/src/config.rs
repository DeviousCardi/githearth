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
    pub trash_retention_seconds: u64,
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
            body_limit_bytes: 16384,
            auth_concurrency: 2,
            storage_root: PathBuf::from("data/repositories"),
            reconcile_interval_seconds: 60,
            reconcile_grace_seconds: 300,
            trash_retention_seconds: 604_800,
        }
    }
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
            || !(3600..=31536000).contains(&self.trash_retention_seconds)
        {
            bail!(
                "configuration bounds: pool 1..100, DB timeout 1..30s, request timeout 1..120s, token TTL 1..86400s, body 1024..1048576 bytes, auth concurrency 1..16, reconcile interval 5..3600s, reconcile grace 30..86400s, trash retention 3600..31536000s"
            );
        }
        if self.storage_root.as_os_str().is_empty() {
            bail!("storage_root must not be empty");
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
}
