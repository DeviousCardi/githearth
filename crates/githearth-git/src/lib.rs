//! Durable repository storage (ADR 0003 layout), push journal, and local reads.
//! No network or push service is exposed.
pub mod browse;
pub mod journal;
pub mod layout;
pub mod merge;

use std::{
    error::Error,
    fmt, io,
    process::{Command, Output, Stdio},
};

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    Invalid(String),
    Git(String),
    TooLarge { size: u64, limit: usize },
    Timeout,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "filesystem or process error: {e}"),
            Self::Invalid(e) => write!(f, "invalid repository data: {e}"),
            Self::Git(e) => write!(f, "Git failed: {e}"),
            Self::TooLarge { size, limit } => write!(f, "blob is {size} bytes; limit is {limit}"),
            Self::Timeout => write!(f, "git process timed out"),
        }
    }
}
impl Error for StorageError {}
impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        // Fixed system locations only (the server environment is not trusted); covers Homebrew too.
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

fn checked(output: Output) -> Result<Vec<u8>, StorageError> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(StorageError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}
