//! Local, operator-only Git read prototype. No network or push service is exposed.
use githearth_core::{GitRef, ObjectId, RepositoryId, RepositoryReader};
use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    Invalid(String),
    Git(String),
    TooLarge { size: u64, limit: usize },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "filesystem or process error: {e}"),
            Self::Invalid(e) => write!(f, "invalid repository data: {e}"),
            Self::Git(e) => write!(f, "Git failed: {e}"),
            Self::TooLarge { size, limit } => write!(f, "blob is {size} bytes; limit is {limit}"),
        }
    }
}
impl Error for StorageError {}
impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct NativeGitStorage {
    root: PathBuf,
}

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
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

impl NativeGitStorage {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: fs::canonicalize(root)?,
        })
    }

    pub fn create(&self, id: &RepositoryId) -> Result<PathBuf, StorageError> {
        let path = self.root.join(format!("{id}.git"));
        // create_dir is exclusive: never silently reinitialize an existing repository.
        fs::create_dir(&path)?;
        checked(
            git()
                .args(["-c", "init.defaultBranch=main", "init", "--bare", "--"])
                .arg(&path)
                .output()?,
        )?;
        Ok(path)
    }

    fn path(&self, id: &RepositoryId) -> Result<PathBuf, StorageError> {
        let path = self.root.join(format!("{id}.git"));
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_dir() {
            return Err(StorageError::Invalid(
                "repository must be a directory, not a symlink".into(),
            ));
        }
        let canonical = fs::canonicalize(&path)?;
        if canonical.parent() != Some(self.root.as_path()) {
            return Err(StorageError::Invalid(
                "repository escaped the configured storage root".into(),
            ));
        }
        Ok(canonical)
    }

    fn run(&self, id: &RepositoryId, args: &[&str]) -> Result<Vec<u8>, StorageError> {
        checked(
            git()
                .arg("--git-dir")
                .arg(self.path(id)?)
                .args(args)
                .output()?,
        )
    }
}

impl RepositoryReader for NativeGitStorage {
    type Error = StorageError;

    fn list_refs(&self, repository: &RepositoryId) -> Result<Vec<GitRef>, StorageError> {
        let bytes = self.run(
            repository,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads/",
                "refs/tags/",
            ],
        )?;
        let text = String::from_utf8(bytes).map_err(|e| StorageError::Invalid(e.to_string()))?;
        text.lines()
            .map(|line| {
                let (oid, name) = line
                    .split_once(' ')
                    .ok_or_else(|| StorageError::Invalid("unexpected ref record".into()))?;
                Ok(GitRef {
                    name: name.to_owned(),
                    target: oid.parse().map_err(|e: githearth_core::InvalidId| {
                        StorageError::Invalid(e.to_string())
                    })?,
                })
            })
            .collect()
    }

    fn read_blob(
        &self,
        repository: &RepositoryId,
        object: &ObjectId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let oid = object.to_string();
        if self.run(repository, &["cat-file", "-t", &oid])? != b"blob\n" {
            return Err(StorageError::Invalid(
                "requested object is not a blob".into(),
            ));
        }
        let size_bytes = self.run(repository, &["cat-file", "-s", &oid])?;
        let size: u64 = String::from_utf8_lossy(&size_bytes)
            .trim()
            .parse()
            .map_err(|_| StorageError::Invalid("invalid blob size".into()))?;
        if size > max_bytes as u64 {
            return Err(StorageError::TooLarge {
                size,
                limit: max_bytes,
            });
        }
        let data = self.run(repository, &["cat-file", "blob", &oid])?;
        if data.len() as u64 != size {
            return Err(StorageError::Invalid("blob length mismatch".into()));
        }
        Ok(data)
    }
}
