//! One-command installation (M4.1, acceptance case U09): from a clean host with
//! a reachable PostgreSQL to a `doctor`-clean installation.
//!
//! Every step detects completed work and leaves it unchanged, so running the
//! command again (or after an interruption) performs only what is still
//! missing. The database URL and the administrator password are read from the
//! environment or standard input, never from command arguments, and neither is
//! ever written to output.
use crate::{MIGRATOR, bootstrap_admin, config::Config, connect, doctor, ssh};
use anyhow::{Context, Result, bail};
use std::{
    io::{self, IsTerminal, Read},
    net::SocketAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Largest input accepted on stdin (the database URL or the password).
const INPUT_LIMIT: u64 = 1026;

/// Name of the host key file `setup` creates when `--ssh-host-key-path` is not
/// given: `<config dir>/ssh_host_key`, the path Compose and the runbooks use.
pub const DEFAULT_HOST_KEY_FILE: &str = "ssh_host_key";

/// Name of the installation key `setup` creates when the configuration does not
/// name one: `<config dir>/installation.key`. It lives beside the configuration,
/// outside `storage_root`, so the storage tar never carries it.
pub const DEFAULT_INSTALLATION_KEY_FILE: &str = "installation.key";

/// Explicit settings for the configuration file `setup` writes.
pub struct Options {
    /// Where to write the configuration; the default is `/etc/githearth/config.toml`.
    pub config: PathBuf,
    /// The database URL. Callers that read it from the environment or stdin
    /// must pass that value here; it is never echoed.
    pub database_url: String,
    pub storage_root: PathBuf,
    pub bind: SocketAddr,
    pub ssh_enabled: bool,
    pub ssh_bind: SocketAddr,
    /// Default: `<config dir>/ssh_host_key`.
    pub ssh_host_key_path: PathBuf,
    /// Largest push the installer is told it must hold on the storage disk.
    pub git_push_limit_bytes: usize,
    /// Flags supplied on this run. When a configuration file already exists
    /// its settings are used, and these are named in the report rather than
    /// dropped without a word.
    pub supplied_flags: Vec<&'static str>,
}

impl Options {
    /// Defaults derived from the configuration path only: nothing is read from
    /// the process environment here, so a `setup` run is fully explained by
    /// its flags and stdin.
    ///
    /// The configuration path is made absolute first, so the default host key
    /// path derived from its directory is absolute too and `serve` can load
    /// the key from any working directory.
    pub fn new(config: PathBuf, database_url: String) -> Result<Self> {
        let config = absolute_path(&config)?;
        let directory = config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        Ok(Self {
            config,
            database_url,
            storage_root: PathBuf::from("/var/lib/githearth/repositories"),
            bind: "127.0.0.1:8080".parse().expect("fixed default address"),
            ssh_enabled: true,
            ssh_bind: "127.0.0.1:2222".parse().expect("fixed default address"),
            ssh_host_key_path: directory.join(DEFAULT_HOST_KEY_FILE),
            git_push_limit_bytes: Config::default().git_push_limit_bytes,
            supplied_flags: Vec::new(),
        })
    }

    /// The configuration `setup` writes and every later step uses. It is
    /// validated as the server would load it.
    fn configuration(&self) -> Result<Config> {
        let mut config = Config {
            storage_root: self.storage_root.clone(),
            bind: self.bind,
            ssh_enabled: self.ssh_enabled,
            ssh_bind: self.ssh_bind,
            ssh_host_key_path: if self.ssh_enabled {
                self.ssh_host_key_path.clone()
            } else {
                PathBuf::new()
            },
            git_push_limit_bytes: self.git_push_limit_bytes,
            installation_key_file: self
                .config
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .join(DEFAULT_INSTALLATION_KEY_FILE),
            ..Config::default()
        };
        if self.database_url.is_empty() {
            bail!(
                "set GITHEARTH_DATABASE_URL or write the connection URL to stdin; \
                 it is not accepted as a command argument"
            );
        }
        config.database_url = self.database_url.clone();
        config.validate()?;
        if let Some(port) = shared_listener_port(&config) {
            bail!("--http-port and --ssh-port must differ; both are {port}");
        }
        Ok(config)
    }
}

/// Resolves `path` against the working directory at the moment `setup` runs.
/// [`std::path::absolute`] normalises the path text without reading the
/// filesystem, so no symlink is followed and the file that is written does not
/// change. The recorded setting then works from any directory a later
/// `serve` or `doctor` happens to run in.
pub fn absolute_path(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path)
        .with_context(|| format!("cannot resolve {} to an absolute path", path.display()))
}

/// HTTP binds first in `serve`, so a shared port would leave the SSH listener
/// unable to bind while `setup` still reported success. Checked on the
/// first-install path and on the recorded-configuration path, because the file
/// can be edited by hand between runs.
fn shared_listener_port(config: &Config) -> Option<u16> {
    (config.ssh_enabled && config.ssh_bind.port() == config.bind.port()).then(|| config.bind.port())
}

/// How the administrator password reaches `run`. It is read only when an
/// administrator still has to be created, so a repeated run never blocks.
pub trait PasswordSource {
    /// Returns the password, or `None` when the operator supplied none.
    fn read_password(&mut self) -> io::Result<Option<String>>;
}

/// Reads the password from stdin: one line, or the whole input when it has no
/// newline. Stdin must not be a terminal, because a terminal would echo the
/// typed password; the caller pipes it instead. Nothing is printed, so the
/// password never reaches arguments, logs or the screen.
pub struct StdinPassword;

impl PasswordSource for StdinPassword {
    fn read_password(&mut self) -> io::Result<Option<String>> {
        if io::stdin().is_terminal() {
            return Err(io::Error::other(
                "stdin is a terminal, and an interactive password would be echoed; \
                 pipe the password to setup on stdin instead, for example \
                 `read -r -s -p 'Password: ' PASSWORD; \
                 printf '%s' \"$PASSWORD\" | githearth-server setup`",
            ));
        }
        let mut input = String::new();
        io::stdin().take(INPUT_LIMIT).read_to_string(&mut input)?;
        if input.trim_end_matches(['\r', '\n']).is_empty() {
            return Ok(None);
        }
        Ok(Some(input))
    }
}

/// Fixed administrator provided by the caller (tests and scripted installs).
pub struct FixedPassword(pub Option<String>);

impl PasswordSource for FixedPassword {
    fn read_password(&mut self) -> io::Result<Option<String>> {
        Ok(self.0.take())
    }
}

/// What one step did.
#[derive(Debug)]
pub enum Outcome {
    Created(String),
    AlreadyPresent(String),
    Skipped(String),
}

impl Outcome {
    fn report(&self) {
        match self {
            Self::Created(detail) => println!("created      {detail}"),
            Self::AlreadyPresent(detail) => println!("unchanged    {detail}"),
            Self::Skipped(detail) => println!("skipped      {detail}"),
        }
    }
}

/// Steps that finished before a failure, so a resumed run reports them too.
/// A step that found its work already done or skipped still counts as done.
pub struct Report {
    pub completed: Vec<String>,
}

/// Runs `setup` and returns the report of the finished installation. Every
/// choice is a flag or a default, and once the configuration file exists its
/// settings are the ones used, so an operator's edits survive a repeated run.
/// The only input is the administrator password, read from piped stdin when an
/// administrator still has to be created.
pub async fn run(options: &Options, password: &mut dyn PasswordSource) -> Result<Report> {
    let mut report = Report {
        completed: Vec::new(),
    };
    let (config, configuration) = match read_existing(&options.config)? {
        Some(existing) => {
            let outcome =
                recorded_settings(&options.config, &existing.config, existing.mode, options);
            (existing.config, outcome)
        }
        None => {
            let config = options.configuration()?;
            let outcome = write_config(&options.config, &config);
            (config, outcome)
        }
    };
    report.step("configuration", configuration)?;
    report.step(
        "installation key",
        installation_key(&config.installation_key_file),
    )?;
    report.step(
        "storage directory",
        prepare_directories(&config.storage_root),
    )?;
    if config.ssh_enabled {
        report.step(
            "SSH host key directory",
            host_key_directory(&config.ssh_host_key_path),
        )?;
        report.step("SSH host key", host_key(&config.ssh_host_key_path))?;
    } else {
        report.step(
            "SSH host key",
            Ok(Outcome::Skipped(
                "SSH is disabled; no host key is generated".into(),
            )),
        )?;
    }

    report.step("schema", migrate_with(&config).await)?;
    report.step("administrator", bootstrap_with(&config, password).await)?;

    let doctor_code = doctor::run(&config).await;
    if doctor_code != 0 {
        bail!(
            "doctor reported failures; fix them and run the command again to finish the remaining steps"
        );
    }
    Ok(report)
}

impl Report {
    /// Reports one step. On failure the error says which step stopped the run
    /// and which steps are already done, so the next run only finishes the rest.
    fn step(&mut self, name: &str, outcome: Result<Outcome>) -> Result<()> {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let done = if self.completed.is_empty() {
                    "no step completed".to_owned()
                } else {
                    format!("completed: {}", self.completed.join(", "))
                };
                // The cause is repeated here because the binary prints only the
                // outermost message; no cause below carries a secret.
                bail!(
                    "setup stopped during the {name} step ({done}); \
                     run it again to resume: {error:#}"
                );
            }
        };
        outcome.report();
        // Every successful step is recorded, not only the ones that created
        // something: on a resumed run the configuration, directories and host
        // key are already present, and a later failure must still name them.
        self.completed.push(name.to_owned());
        Ok(())
    }
}

/// The configuration already on disk, with the mode that was actually read:
/// the report prints this value rather than a constant.
struct Existing {
    config: Config,
    mode: u32,
}

/// The configuration already on disk, or `None` when the file does not exist.
/// Once it exists it is the source of truth: an operator's later edits are used
/// by every step instead of the values this invocation would have written.
fn read_existing(path: &Path) -> Result<Option<Existing>> {
    let Some(mode) = checked_config_file(path)? else {
        return Ok(None);
    };
    // Parsed as written, without the GITHEARTH_* environment overrides:
    // the file is what this installation records, and comparing the URL
    // there is what stops a run from migrating one database while
    // recording another. `serve` still applies the overrides.
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let config: Config = toml::from_str(&text).map_err(|_| {
        anyhow::anyhow!(
            "the existing configuration file {} is not valid TOML; it was left unchanged",
            path.display()
        )
    })?;
    config.validate().map_err(|error| {
        anyhow::anyhow!(
            "the existing configuration file {} is not a usable configuration \
             and was left unchanged: {error:#}",
            path.display()
        )
    })?;
    Ok(Some(Existing { config, mode }))
}

/// The database URL recorded in an existing configuration, after the same
/// checks `run` applies. `None` when no configuration exists yet. A resumed
/// run uses this instead of reading (or prompting for) the URL again.
pub fn recorded_database_url(path: &Path) -> Result<Option<String>> {
    Ok(read_existing(path)?.map(|existing| existing.config.database_url))
}

/// Inspection shared by every path that reads or accepts an existing
/// configuration: `None` when nothing exists, otherwise the file's mode after
/// it is proved to be a regular file, owned by the current user, that group
/// and others cannot read. The file is never modified, so a mode that is
/// printed later is one that was read.
fn checked_config_file(path: &Path) -> Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("cannot inspect {}: {e}", path.display()),
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "{} is a symbolic link; refusing to use it. Replace it with a regular file",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("{} exists and is not a configuration file", path.display());
    }
    check_owner(path, current_uid())?;
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "{} has mode {mode:04o}; group or others could read the database URL. \
             Fix it with `chmod 0600 {}` and run the command again",
            path.display(),
            path.display()
        );
    }
    Ok(Some(mode))
}

/// Accepts an existing configuration only when it describes the database this
/// run was told to install; every other recorded setting is used as it stands.
/// `mode` is the mode read by `read_existing`, so the report never claims one
/// the tool did not observe.
fn recorded_settings(
    path: &Path,
    recorded: &Config,
    mode: u32,
    options: &Options,
) -> Result<Outcome> {
    if recorded.database_url != options.database_url {
        bail!(
            "{} records a different database URL from the one supplied for this run; \
             it was not changed. If the supplied URL is the intended one, correct \
             `database_url` in the file and run the command again; otherwise use the \
             recorded URL",
            path.display()
        );
    }
    // The first-install check cannot see a hand-edited or pre-existing file;
    // without this a resumed run would print success while `serve` could not
    // bind SSH. The message names the recorded settings, not flags that a
    // resumed run ignores.
    if let Some(port) = shared_listener_port(recorded) {
        bail!(
            "{} records bind and ssh_bind on the same port {port}; HTTP and SSH \
             cannot share a port. Edit the file so the two ports differ",
            path.display()
        );
    }
    Ok(Outcome::AlreadyPresent(format!(
        "{} (mode {mode:04o}, existing settings used{})",
        path.display(),
        ignored_flags_note(path, options)
    )))
}

/// Names the flags this run supplied whose values the existing configuration
/// makes irrelevant, so a supplied value is never ignored without a word.
fn ignored_flags_note(path: &Path, options: &Options) -> String {
    if options.supplied_flags.is_empty() {
        return String::new();
    }
    format!(
        "; unused flags: {} ({} records these settings)",
        options.supplied_flags.join(", "),
        path.display()
    )
}

/// Writes the configuration with mode 0600 through a private temporary file and
/// an atomic rename; an existing file is never replaced. An existing file is
/// inspected before it is read and its values are compared with this run, so a
/// loose mode, a symlink or a concurrent `setup` with different settings is
/// refused instead of quietly accepted or overwritten.
fn write_config(path: &Path, config: &Config) -> Result<Outcome> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // The mode reported below is the one read here, never an assumption.
    let existing_mode = checked_config_file(path)?;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let created_directory = create_private_dir(directory)?;
    let contents = render(config);
    let directory_note = match created_directory.created {
        Some(created) => format!(
            ", directory {} created {}",
            created.display(),
            read_mode(&created)
        ),
        None => String::new(),
    };

    if let Some(mode) = existing_mode {
        let existing: Config = toml::from_str(
            &std::fs::read_to_string(path)
                .context("cannot read the existing configuration file")?,
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "the existing configuration file {} is not a valid configuration; \
                 it was left unchanged",
                path.display()
            )
        })?;
        let mismatch = [
            ("database_url", existing.database_url != config.database_url),
            ("storage_root", existing.storage_root != config.storage_root),
            ("bind", existing.bind != config.bind),
            ("ssh_enabled", existing.ssh_enabled != config.ssh_enabled),
            ("ssh_bind", existing.ssh_bind != config.ssh_bind),
            (
                "ssh_host_key_path",
                existing.ssh_host_key_path != config.ssh_host_key_path,
            ),
            (
                "installation_key_file",
                existing.installation_key_file != config.installation_key_file,
            ),
            (
                "git_push_limit_bytes",
                existing.git_push_limit_bytes != config.git_push_limit_bytes,
            ),
        ]
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
        if !mismatch.is_empty() {
            bail!(
                "{} already exists with different settings ({}); it was not changed. \
                 Edit it, or remove it to write a new one",
                path.display(),
                mismatch.join(", ")
            );
        }
        return Ok(Outcome::AlreadyPresent(format!(
            "{} (mode {mode:04o}, existing settings match{directory_note})",
            path.display()
        )));
    }

    // create_new makes "never overwrite" atomic; the mode applies at creation,
    // so the database URL is never readable by others, even briefly.
    let temporary = directory.join(format!(".githearth-setup-{}.tmp", Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("cannot create a file in {}", directory.display()))?;
    let result = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .context("cannot write the configuration file")
        .and_then(|()| link_new(&temporary, path));
    let _ = std::fs::remove_file(&temporary);
    if !result? {
        return write_config(path, config);
    }
    Ok(Outcome::Created(format!(
        "{} ({}, database URL from the environment or stdin{directory_note})",
        path.display(),
        read_mode(path)
    )))
}

/// The mode of `path`, read now, for a report line. A mode that cannot be read
/// is reported as such, instead of as a number the tool never observed.
fn read_mode(path: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => format!("mode {:04o}", metadata.permissions().mode() & 0o7777),
        Err(e) => format!("mode could not be read ({e})"),
    }
}

/// Publishes `source` under `destination` only when `destination` does not
/// exist yet. `false` means a concurrent `setup` won the race: the caller then
/// verifies that the file it wrote has the same settings, so one installation
/// is never written twice with different values.
fn link_new(source: &Path, destination: &Path) -> Result<bool> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => std::fs::rename(source, destination)
            .map(|()| true)
            .with_context(|| format!("cannot create {}", destination.display())),
    }
}

/// The configuration file in the sample's layout: the values an operator may
/// need to change, one per line, and nothing that `setup` would have to guess.
fn render(config: &Config) -> String {
    let mut out = String::new();
    out.push_str("# Written by githearth-server setup. Mode 0600: it holds the database URL.\n");
    out.push_str("database_url = ");
    out.push_str(&toml_string(&config.database_url));
    out.push('\n');
    let rows: [(&str, String); 9] = [
        ("bind", toml_string(&config.bind.to_string())),
        (
            "storage_root",
            toml_string(&path_text(&config.storage_root)),
        ),
        (
            "database_max_connections",
            config.database_max_connections.to_string(),
        ),
        (
            "database_timeout_seconds",
            config.database_timeout_seconds.to_string(),
        ),
        (
            "request_timeout_seconds",
            config.request_timeout_seconds.to_string(),
        ),
        ("token_ttl_seconds", config.token_ttl_seconds.to_string()),
        ("body_limit_bytes", config.body_limit_bytes.to_string()),
        ("secure_cookies", config.secure_cookies.to_string()),
        ("auth_concurrency", config.auth_concurrency.to_string()),
    ];
    for (name, value) in rows {
        out.push_str(&format!("{name} = {value}\n"));
    }
    let rows: [(&str, String); 7] = [
        (
            "git_push_limit_bytes",
            config.git_push_limit_bytes.to_string(),
        ),
        ("ssh_enabled", config.ssh_enabled.to_string()),
        ("ssh_bind", toml_string(&config.ssh_bind.to_string())),
        (
            "ssh_host_key_path",
            toml_string(&path_text(&config.ssh_host_key_path)),
        ),
        (
            "installation_key_file",
            toml_string(&path_text(&config.installation_key_file)),
        ),
        ("job_concurrency", config.job_concurrency.to_string()),
        (
            "job_timeout_seconds",
            config.job_timeout_seconds.to_string(),
        ),
    ];
    for (name, value) in rows {
        out.push_str(&format!("{name} = {value}\n"));
    }
    out
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A TOML basic string. `Config::load` rejects control characters through the
/// parser, but escaping them here keeps this function total.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Refuses a path owned by a user other than `uid`; the caller passes the
/// effective user id, so the binary never trusts a value from the environment.
fn check_owner(path: &Path, uid: u32) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => bail!("cannot inspect {}: {e}", path.display()),
    };
    if metadata.uid() != uid {
        bail!(
            "{} is owned by user id {}, not by the current user ({}); \
             installations belong to the account that runs the server",
            path.display(),
            metadata.uid(),
            uid
        );
    }
    Ok(())
}

/// Creates `path` and any missing parents with mode 0700 and refuses a
/// directory that someone else owns, or that group or others could write to.
/// Refusal names only the path and the reason.
fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn create_private_dir(path: &Path) -> Result<DirOutcome> {
    create_private_dir_for(path, current_uid())
}

/// What `create_private_dir_for` did: the first directory it created, if any,
/// and the mode it read for the directory that was asked for. Callers report
/// `mode`, so a line about a directory never states a mode the tool assumed.
#[derive(Debug)]
struct DirOutcome {
    created: Option<PathBuf>,
    mode: u32,
}

/// `create_private_dir` for a stated owner, so the refusal below can be tested
/// without owning a second account.
fn create_private_dir_for(path: &Path, uid: u32) -> Result<DirOutcome> {
    use std::os::unix::fs::PermissionsExt;
    let mut created = None;
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        // A relative path's ancestors end with the empty path. Creating it
        // fails with `cannot create : No such file or directory`, which names
        // nothing; every real component below it is created instead.
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(ancestor),
            Err(e) => bail!("cannot inspect {}: {e}", ancestor.display()),
        }
    }
    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => {
                if created.is_none() {
                    created = Some(directory.to_path_buf());
                }
            }
            // A concurrent setup created it first; the checks below cover it.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("cannot create {}", directory.display()));
            }
        }
        // Set the mode before anything is written inside, whatever the umask.
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot set the mode of {}", directory.display()))?;
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "{} is a symbolic link; use a real directory so its permissions can be checked",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!("{} exists and is not a directory", path.display());
    }
    check_owner(path, uid)?;
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o022 != 0 {
        bail!(
            "{} has mode {mode:04o}; group or others could write to it. \
             Fix it with `chmod 0700 {}` and run the command again",
            path.display(),
            path.display()
        );
    }
    Ok(DirOutcome { created, mode })
}

/// The directory that must hold the host key: the key's own directory, or the
/// directory of the configuration when the key path is empty (SSH disabled).
fn key_directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Creates the directory that will hold the host key, with the same ownership
/// and permission checks as the storage root. Never changes the mode of an
/// existing directory.
fn host_key_directory(path: &Path) -> Result<Outcome> {
    let directory = key_directory_of(path);
    let DirOutcome { created, mode } = create_private_dir(directory)?;
    Ok(match created {
        Some(_) => Outcome::Created(format!("{} (mode {mode:04o})", directory.display())),
        None => Outcome::AlreadyPresent(format!(
            "{} (mode {mode:04o}, owned by the current user)",
            directory.display()
        )),
    })
}

fn prepare_directories(path: &Path) -> Result<Outcome> {
    let label = path.display();
    let DirOutcome { created, mode } = create_private_dir(path)?;
    match created {
        Some(_) => Ok(Outcome::Created(format!("{label} (mode {mode:04o})"))),
        None => Ok(Outcome::AlreadyPresent(format!(
            "{label} (mode {mode:04o}, owned by the current user)"
        ))),
    }
}

/// Generates the installation key beside the configuration when it is absent,
/// and checks the mode and ownership when it is present. The key protects every
/// stored secret; a loose key file fails the step rather than being repaired
/// silently.
fn installation_key(path: &Path) -> Result<Outcome> {
    if path.as_os_str().is_empty() {
        bail!(
            "the configuration does not name installation_key_file; add \
             installation_key_file = \"<path>\" and run setup again"
        );
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            crate::secrets::generate_raw_key_file(path)?;
            return Ok(Outcome::Created(format!(
                "{} (mode 0600, outside storage_root; keep it out of backups)",
                path.display()
            )));
        }
        Err(e) => bail!("cannot inspect {}: {e}", path.display()),
    };
    if metadata.file_type().is_symlink() {
        bail!("{} is a symbolic link; refusing to use it", path.display());
    }
    crate::secrets::check_private_file(path, "installation key file")?;
    Ok(Outcome::AlreadyPresent(format!(
        "{} (mode 0600, owned by the current user)",
        path.display()
    )))
}

fn host_key(path: &Path) -> Result<Outcome> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Errors name the path and the reason; generate_host_key never overwrites.
            ssh::generate_host_key(path)?;
            return Ok(Outcome::Created(format!("{} (Ed25519)", path.display())));
        }
        Err(e) => bail!("cannot inspect {}: {e}", path.display()),
    };
    if metadata.file_type().is_symlink() {
        bail!("{} is a symbolic link; refusing to use it", path.display());
    }
    if !metadata.is_file() {
        bail!("{} exists and is not a file", path.display());
    }
    check_owner(path, current_uid())?;
    {
        use std::os::unix::fs::PermissionsExt;
        // The host key is a private key: the same rule as the configuration,
        // which holds the database URL. A generated key is always 0600.
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o077 != 0 {
            bail!(
                "{} has mode {mode:04o}; group or others could read the private host key. \
                 Fix it with `chmod 0600 {}` and run the command again",
                path.display(),
                path.display()
            );
        }
    }
    if rustix::fs::access(path, rustix::fs::Access::READ_OK).is_err() {
        bail!(
            "{} exists but is not readable by the current user; it was not changed",
            path.display()
        );
    }
    // An operator-supplied key is used as it is; only a broken one is refused,
    // with a message that never repeats key material.
    match russh::keys::load_secret_key(path, None) {
        Ok(_) => Ok(Outcome::AlreadyPresent(format!(
            "{} (existing key kept)",
            path.display()
        ))),
        Err(_) => bail!(
            "{} exists but is not a private SSH host key this server can load; \
             remove it to generate a new one",
            path.display()
        ),
    }
}

async fn migrate_with(config: &Config) -> Result<Outcome> {
    let pool = connect(config).await?;
    let outcome = migrate(&pool, config.housekeeping_pin_retention_days).await;
    pool.close().await;
    outcome
}

async fn bootstrap_with(config: &Config, source: &mut dyn PasswordSource) -> Result<Outcome> {
    let pool = connect(config).await?;
    let outcome = bootstrap(&pool, source).await;
    pool.close().await;
    outcome
}

async fn migrate(pool: &sqlx::PgPool, pin_retention_days: u64) -> Result<Outcome> {
    let before: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await
            .unwrap_or(None);
    // The migrator is idempotent: applied migrations are skipped by checksum.
    MIGRATOR.run(pool).await.map_err(|_| {
        anyhow::anyhow!("migration failed; inspect database availability and migration history")
    })?;
    crate::pins::extend_backfilled_pins(pool, pin_retention_days).await?;
    let latest: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    Ok(match before {
        Some(version) if version == latest => {
            Outcome::AlreadyPresent(format!("up to date (latest v{latest})"))
        }
        _ => Outcome::Created(format!("migrated to v{latest}")),
    })
}

/// Creates the first administrator only when none exists. The password comes
/// from `source` and never from arguments; a failed attempt is reported without
/// the driver error, which could carry connection details.
async fn bootstrap(pool: &sqlx::PgPool, source: &mut dyn PasswordSource) -> Result<Outcome> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE is_admin)")
        .fetch_one(pool)
        .await
        .map_err(|_| {
            anyhow::anyhow!("cannot read the accounts table; did the migration succeed?")
        })?;
    if exists {
        return Ok(Outcome::AlreadyPresent(
            "administrator exists (no new account or password)".into(),
        ));
    }
    let password = source
        .read_password()
        .context("cannot read the administrator password from stdin")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no administrator exists and no password was read; \
                 pipe one to setup on stdin (12+ bytes)"
            )
        })?;
    if password.len() > 1025 {
        bail!("password input exceeds limit");
    }
    let password = password.trim_end_matches(['\r', '\n']).to_owned();
    match bootstrap_admin(pool, "admin", password).await {
        // A concurrent setup won the race; bootstrap_admin serialises the two
        // under an advisory lock, so nothing was partially written. The
        // account keeps the password that setup supplied.
        Err(error)
            if error
                .to_string()
                .starts_with("an administrator already exists") =>
        {
            Ok(Outcome::AlreadyPresent(
                "administrator exists (created by another setup)".into(),
            ))
        }
        Err(error) => {
            let message = error.to_string();
            if message.starts_with("username must") || message.starts_with("password must") {
                bail!("{message}");
            }
            bail!("bootstrap failed; check schema, database, and username availability")
        }
        Ok(id) => Ok(Outcome::Created(format!(
            "first administrator `admin` ({id})"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp() -> tempfile::TempDir {
        let directory = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn config_for(root: &Path, database_url: &str) -> Config {
        Config {
            database_url: database_url.into(),
            storage_root: root.join("repositories"),
            ssh_enabled: true,
            ssh_host_key_path: root.join("ssh_host_key"),
            ..Config::default()
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn toml_string_escapes_only_what_toml_requires() {
        assert_eq!(toml_string("postgres://host/db"), "\"postgres://host/db\"");
        assert_eq!(toml_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(toml_string("a\nb\tc"), "\"a\\nb\\tc\"");
        assert_eq!(toml_string("a\u{1}b"), "\"a\\u0001b\"");
    }

    #[test]
    fn key_directory_is_the_parent_or_the_current_directory() {
        assert_eq!(
            key_directory_of(Path::new("/etc/githearth/key")),
            Path::new("/etc/githearth")
        );
        assert_eq!(key_directory_of(Path::new("key")), Path::new("."));
        // The filesystem root has no parent; "." keeps the function total.
        assert_eq!(key_directory_of(Path::new("/")), Path::new("."));
    }

    #[test]
    fn configuration_defaults_put_everything_under_the_config_path() {
        let options = Options::new(
            PathBuf::from("/etc/githearth/config.toml"),
            "postgres://db".into(),
        )
        .unwrap();
        assert_eq!(
            options.ssh_host_key_path,
            Path::new("/etc/githearth/ssh_host_key")
        );
        assert!(options.ssh_enabled, "SSH is on by default");
        assert_eq!(options.bind.to_string(), "127.0.0.1:8080");
        assert_eq!(options.ssh_bind.to_string(), "127.0.0.1:2222");
        assert_eq!(
            options.storage_root,
            Path::new("/var/lib/githearth/repositories")
        );

        // A relative configuration path is resolved against the working
        // directory at the moment setup runs, and the default host key path
        // follows the resolved configuration so it is CWD-independent too.
        let cwd = std::env::current_dir().unwrap();
        let options = Options::new(PathBuf::from("config.toml"), "postgres://db".into()).unwrap();
        assert_eq!(options.config, cwd.join("config.toml"));
        assert_eq!(options.ssh_host_key_path, cwd.join("ssh_host_key"));
    }

    #[test]
    fn the_requested_ssh_bind_is_recorded() {
        let directory = temp();
        let mut options = Options::new(
            directory.path().join("config.toml"),
            "postgres://db/name".into(),
        )
        .unwrap();
        options.ssh_bind = "127.0.0.1:2299".parse().unwrap();
        options.bind = "127.0.0.1:18081".parse().unwrap();
        let config = options.configuration().unwrap();
        assert_eq!(config.ssh_bind.to_string(), "127.0.0.1:2299");
        assert_eq!(config.bind.to_string(), "127.0.0.1:18081");

        // What is written is what `Config::load` reads back.
        let path = directory.path().join("config.toml");
        write_config(&path, &config).unwrap();
        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(reloaded.ssh_bind.to_string(), "127.0.0.1:2299");
    }

    #[test]
    fn equal_http_and_ssh_ports_are_refused() {
        let directory = temp();
        let mut options = Options::new(
            directory.path().join("config.toml"),
            "postgres://db/name".into(),
        )
        .unwrap();
        options.bind = "127.0.0.1:18080".parse().unwrap();
        options.ssh_bind = "127.0.0.1:18080".parse().unwrap();
        // The message names both flags, so the operator knows what to change.
        let error = match options.configuration() {
            Ok(_) => panic!("equal HTTP and SSH ports must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("--http-port"), "{error}");
        assert!(error.contains("--ssh-port"), "{error}");
        assert!(error.contains("must differ"), "{error}");

        // With SSH disabled no listener is started on that port, so it is not
        // the same conflict.
        options.ssh_enabled = false;
        assert!(options.configuration().is_ok());
    }

    #[test]
    fn an_empty_database_url_is_refused_before_anything_is_written() {
        let options = Options::new(PathBuf::from("config.toml"), String::new()).unwrap();
        let error = match options.configuration() {
            Ok(_) => panic!("an empty database URL must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("GITHEARTH_DATABASE_URL"), "{error}");
    }

    #[test]
    fn disabled_ssh_writes_an_empty_host_key_path() {
        let directory = temp();
        let mut options = Options::new(
            directory.path().join("config.toml"),
            "postgres://db/name".into(),
        )
        .unwrap();
        options.ssh_enabled = false;
        let config = options.configuration().unwrap();
        assert!(!config.ssh_enabled);
        assert!(config.ssh_host_key_path.as_os_str().is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn the_written_configuration_round_trips_with_mode_0600() {
        let directory = temp();
        let path = directory.path().join("etc/config.toml");
        let config = config_for(directory.path(), "postgres://user:secret@host/db");
        let outcome = write_config(&path, &config).unwrap();
        let Outcome::Created(detail) = outcome else {
            panic!("a missing configuration is created");
        };
        // The line states the mode the file has, which is the mode that was read.
        assert!(
            detail.contains(&format!("mode {:04o}", mode(&path))),
            "{detail}"
        );
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&directory.path().join("etc")), 0o700);

        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(reloaded.database_url, config.database_url);
        assert_eq!(reloaded.storage_root, config.storage_root);
        assert_eq!(reloaded.ssh_host_key_path, config.ssh_host_key_path);
        assert_eq!(reloaded.bind, config.bind);
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(directory.path().join("etc"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn an_existing_configuration_with_the_same_settings_is_unchanged() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let config = config_for(directory.path(), "postgres://user:secret@host/db");
        assert!(matches!(
            write_config(&path, &config).unwrap(),
            Outcome::Created(_)
        ));
        let before = std::fs::read(&path).unwrap();
        let outcome = write_config(&path, &config).unwrap();
        assert!(matches!(outcome, Outcome::AlreadyPresent(_)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn an_existing_configuration_with_other_settings_is_refused_by_name() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let config = config_for(directory.path(), "postgres://user:first-secret@host/db");
        write_config(&path, &config).unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut changed = config.clone();
        changed.storage_root = directory.path().join("elsewhere");
        changed.database_url = "postgres://user:second-secret@host/db".into();
        let error = write_config(&path, &changed).unwrap_err().to_string();
        assert!(
            error.contains("already exists with different settings"),
            "{error}"
        );
        assert!(
            error.contains("storage_root") && error.contains("database_url"),
            "{error}"
        );
        assert!(
            !error.contains("secret"),
            "the URL and its password stay unprinted: {error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn an_unreadable_or_symlinked_configuration_is_refused() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "not toml").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = config_for(directory.path(), "postgres://host/db");
        let error = write_config(&path, &config).unwrap_err().to_string();
        assert!(error.contains("is not a valid configuration"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not toml");

        let link = directory.path().join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let error = write_config(&link, &config).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn private_directories_are_created_nested_and_world_writable_ones_are_refused() {
        let directory = temp();
        let nested = directory.path().join("a/b/c");
        assert!(create_private_dir(&nested).unwrap().created.is_some());
        assert_eq!(mode(&nested), 0o700);
        assert!(create_private_dir(&nested).unwrap().created.is_none());

        let open = directory.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = create_private_dir(&open).unwrap_err().to_string();
        assert!(
            error.contains("group or others could write to it"),
            "{error}"
        );
        assert!(error.contains("chmod 0700"), "{error}");

        let group = directory.path().join("group");
        std::fs::create_dir(&group).unwrap();
        std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o770)).unwrap();
        let error = create_private_dir(&group).unwrap_err().to_string();
        assert!(
            error.contains("group or others could write to it"),
            "{error}"
        );
        // The refusal never changes permissions.
        assert_eq!(mode(&group), 0o770);

        let file = directory.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let error = create_private_dir(&file).unwrap_err().to_string();
        assert!(error.contains("is not a directory"), "{error}");
    }

    #[test]
    fn a_directory_line_states_the_mode_that_was_read() {
        let directory = temp();
        // 0755 is accepted (only the write bits are checked), so a line that
        // printed a constant 0700 would be a claim the tool never verified.
        let shared = directory.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        let outcome = create_private_dir(&shared).unwrap();
        assert_eq!(outcome.mode, 0o755, "the reported mode is the one read");
        assert!(outcome.created.is_none());

        let Outcome::AlreadyPresent(detail) = prepare_directories(&shared).unwrap() else {
            panic!("an existing directory is unchanged");
        };
        assert!(detail.contains("mode 0755"), "{detail}");
        assert!(!detail.contains("mode 0700"), "{detail}");

        let Outcome::AlreadyPresent(detail) = host_key_directory(&shared.join("key")).unwrap()
        else {
            panic!("an existing key directory is unchanged");
        };
        assert!(detail.contains("mode 0755"), "{detail}");
    }

    #[test]
    fn a_mode_that_cannot_be_read_is_never_printed_as_a_number() {
        let directory = temp();
        let text = read_mode(&directory.path().join("missing"));
        assert!(text.contains("could not be read"), "{text}");
        assert!(!text.contains("mode 0"), "{text}");
    }

    #[test]
    fn a_directory_owned_by_another_user_is_refused() {
        let directory = temp();
        let owned = directory.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        let other = current_uid().wrapping_add(1);
        let error = create_private_dir_for(&owned, other)
            .unwrap_err()
            .to_string();
        assert!(error.contains("is owned by user id"), "{error}");
        assert!(error.contains("not by the current user"), "{error}");
        // The actual owner is accepted; nothing about the path changed.
        assert!(create_private_dir(&owned).unwrap().created.is_none());
        // A file that must not be replaced is refused for the same reason.
        let config = directory.path().join("config.toml");
        std::fs::write(&config, "x").unwrap();
        let error = check_owner(&config, other).unwrap_err().to_string();
        assert!(error.contains("is owned by user id"), "{error}");
        assert!(check_owner(&config, current_uid()).is_ok());
        assert!(
            check_owner(&directory.path().join("missing"), current_uid()).is_ok(),
            "a missing path has no owner to check yet"
        );
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let directory = temp();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = create_private_dir(&link).unwrap_err().to_string();
        assert!(error.contains("is a symbolic link"), "{error}");
    }

    #[test]
    fn an_existing_configuration_is_read_as_written() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        assert!(
            read_existing(&path).unwrap().is_none(),
            "no file, no config"
        );

        let mut config = config_for(directory.path(), "postgres://host/first");
        config.secure_cookies = false;
        write_config(&path, &config).unwrap();
        // A process-environment override must not change what the file records.
        let read = read_existing(&path).unwrap().unwrap();
        assert_eq!(read.config.database_url, "postgres://host/first");
        assert!(!read.config.secure_cookies);
        assert_eq!(read.config.storage_root, config.storage_root);
        assert_eq!(read.mode, 0o600, "the reported mode is the one read");
        assert_eq!(
            recorded_database_url(&path).unwrap().as_deref(),
            Some("postgres://host/first")
        );
        assert!(
            recorded_database_url(&directory.path().join("none"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_group_or_other_readable_configuration_is_refused_with_the_mode_read() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let config = config_for(directory.path(), "postgres://user:secret@host/db");
        write_config(&path, &config).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::read(&path).unwrap();

        for error in [
            read_existing(&path).err().unwrap().to_string(),
            write_config(&path, &config).unwrap_err().to_string(),
        ] {
            assert!(error.contains("has mode 0644"), "{error}");
            assert!(error.contains("chmod 0600"), "{error}");
            assert!(
                !error.contains("secret"),
                "the URL stays unprinted: {error}"
            );
        }
        assert_eq!(mode(&path), 0o644, "the mode is refused, not changed");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // A private file is accepted and its mode is what the report prints.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let outcome = recorded_settings(
            &path,
            &config,
            0o600,
            &Options::new(path.clone(), config.database_url.clone()).unwrap(),
        )
        .unwrap();
        let Outcome::AlreadyPresent(detail) = outcome else {
            panic!("an existing private configuration is accepted");
        };
        assert!(detail.contains("mode 0600"), "{detail}");
    }

    #[test]
    fn a_symlinked_configuration_is_refused_by_every_reader() {
        let directory = temp();
        let real = directory.path().join("real.toml");
        let config = config_for(directory.path(), "postgres://host/db");
        write_config(&real, &config).unwrap();
        let link = directory.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        for error in [
            read_existing(&link).err().unwrap().to_string(),
            write_config(&link, &config).unwrap_err().to_string(),
            recorded_database_url(&link).unwrap_err().to_string(),
        ] {
            assert!(error.contains("is a symbolic link"), "{error}");
        }
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink is left in place"
        );
        assert_eq!(std::fs::read(&real).unwrap(), render(&config).as_bytes());
    }

    #[test]
    fn an_existing_configuration_must_describe_the_supplied_database() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let recorded = config_for(directory.path(), "postgres://host/first");
        let options = Options::new(path.clone(), "postgres://host/first".into()).unwrap();
        let outcome = recorded_settings(&path, &recorded, 0o600, &options).unwrap();
        let Outcome::AlreadyPresent(detail) = outcome else {
            panic!("the same database is accepted");
        };
        assert!(detail.contains("mode 0600"), "{detail}");

        let options = Options::new(path.clone(), "postgres://host/second".into()).unwrap();
        let error = recorded_settings(&path, &recorded, 0o600, &options)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("records a different database URL"),
            "{error}"
        );
        assert!(
            !error.contains("first") && !error.contains("second"),
            "{error}"
        );
    }

    #[test]
    fn an_existing_configuration_with_equal_listener_ports_is_refused() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let mut recorded = config_for(directory.path(), "postgres://host/db");
        recorded.ssh_bind = recorded.bind;
        let options = Options::new(path.clone(), "postgres://host/db".into()).unwrap();
        let error = recorded_settings(&path, &recorded, 0o600, &options)
            .unwrap_err()
            .to_string();
        assert!(error.contains("bind and ssh_bind"), "{error}");
        assert!(error.contains("same port"), "{error}");
        assert!(error.contains(&recorded.bind.port().to_string()), "{error}");

        // With SSH disabled the SSH port is unused, so a shared number is fine.
        recorded.ssh_enabled = false;
        let outcome = recorded_settings(&path, &recorded, 0o600, &options).unwrap();
        assert!(matches!(outcome, Outcome::AlreadyPresent(_)));
    }

    #[test]
    fn a_supplied_flag_is_named_when_the_existing_configuration_overrides_it() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let recorded = config_for(directory.path(), "postgres://host/db");
        let mut options = Options::new(path.clone(), "postgres://host/db".into()).unwrap();
        options.supplied_flags = vec!["--http-port", "--storage-root"];
        let outcome = recorded_settings(&path, &recorded, 0o600, &options).unwrap();
        let Outcome::AlreadyPresent(detail) = outcome else {
            panic!("the same database is accepted");
        };
        assert!(
            detail.contains("unused flags: --http-port, --storage-root"),
            "{detail}"
        );
        assert!(detail.contains("records these settings"), "{detail}");
        assert!(detail.contains("existing settings used"), "{detail}");
    }

    #[test]
    fn a_recorded_configuration_that_is_not_usable_is_refused_when_read() {
        let directory = temp();
        let path = directory.path().join("config.toml");
        let mut config = config_for(directory.path(), "postgres://host/db");
        config.database_url = "mysql://host/db".into();
        std::fs::write(&path, render(&config)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = read_existing(&path).err().unwrap().to_string();
        assert!(error.contains("is not a usable configuration"), "{error}");
        assert!(error.contains("PostgreSQL URL"), "{error}");
    }

    #[test]
    fn a_host_key_is_generated_once_and_kept_thereafter() {
        let directory = temp();
        let path = directory.path().join("keys/ssh_host_key");
        create_private_dir(path.parent().unwrap()).unwrap();
        assert!(matches!(host_key(&path).unwrap(), Outcome::Created(_)));
        assert_eq!(mode(&path), 0o600);
        let first = std::fs::read(&path).unwrap();
        assert!(matches!(
            host_key(&path).unwrap(),
            Outcome::AlreadyPresent(_)
        ));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            first,
            "an existing key is kept"
        );
    }

    #[test]
    fn a_file_that_is_not_a_key_is_refused_and_kept() {
        let directory = temp();
        let path = directory.path().join("ssh_host_key");
        std::fs::write(&path, "not a key").unwrap();
        // A private mode, so this reaches the key-loading check rather than the
        // mode refusal that a fresh file's umask-derived 0644 would trigger.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = host_key(&path).unwrap_err().to_string();
        assert!(error.contains("is not a private SSH host key"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key");

        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let error = host_key(&link).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");

        let as_directory = directory.path().join("directory");
        std::fs::create_dir(&as_directory).unwrap();
        let error = host_key(&as_directory).unwrap_err().to_string();
        assert!(error.contains("is not a file"), "{error}");
    }

    #[test]
    fn a_group_or_other_readable_host_key_is_refused_with_the_mode_read() {
        let directory = temp();
        let path = directory.path().join("ssh_host_key");
        assert!(matches!(host_key(&path).unwrap(), Outcome::Created(_)));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::read(&path).unwrap();

        let error = host_key(&path).unwrap_err().to_string();
        assert!(error.contains("has mode 0644"), "{error}");
        assert!(error.contains("chmod 0600"), "{error}");
        assert_eq!(mode(&path), 0o644, "the mode is refused, not changed");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // The remedy works and the existing key is then kept.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            host_key(&path).unwrap(),
            Outcome::AlreadyPresent(_)
        ));
    }

    #[test]
    fn the_report_names_the_step_that_stopped_the_run() {
        let mut report = Report {
            completed: Vec::new(),
        };
        report
            .step("configuration", Ok(Outcome::Created("c".into())))
            .unwrap();
        let error = report
            .step("schema", Err(anyhow::anyhow!("cannot connect")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("stopped during the schema step"), "{error}");
        assert!(error.contains("completed: configuration"), "{error}");
        assert!(error.contains("cannot connect"), "{error}");
        assert!(error.contains("run it again to resume"), "{error}");

        let mut empty = Report {
            completed: Vec::new(),
        };
        let error = empty
            .step("configuration", Err(anyhow::anyhow!("nope")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no step completed"), "{error}");
    }

    #[test]
    fn a_resumed_failure_names_the_steps_that_were_already_done() {
        let mut report = Report {
            completed: Vec::new(),
        };
        report
            .step("configuration", Ok(Outcome::AlreadyPresent("cfg".into())))
            .unwrap();
        report
            .step(
                "storage directory",
                Ok(Outcome::AlreadyPresent("store".into())),
            )
            .unwrap();
        report
            .step("SSH host key", Ok(Outcome::Skipped("ssh off".into())))
            .unwrap();
        let error = report
            .step("schema", Err(anyhow::anyhow!("cannot connect")))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("completed: configuration, storage directory, SSH host key"),
            "{error}"
        );
        assert!(!error.contains("no step completed"), "{error}");
    }

    #[tokio::test]
    async fn a_password_is_required_only_when_no_administrator_exists() {
        // No database: the existence query fails before stdin is read, which is
        // the branch this asserts (the integration tests cover the rest).
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost:1/unused").unwrap();
        let mut source = FixedPassword(Some("a long enough password".into()));
        assert!(bootstrap(&pool, &mut source).await.is_err());
        assert!(
            source.0.is_some(),
            "the password must not be consumed before the account query"
        );
    }

    #[tokio::test]
    async fn the_fixed_password_source_is_read_once() {
        let mut source = FixedPassword(None);
        assert!(source.read_password().unwrap().is_none());
        let mut source = FixedPassword(Some("secret".into()));
        assert_eq!(source.read_password().unwrap().as_deref(), Some("secret"));
        assert!(source.read_password().unwrap().is_none(), "read once");
    }
}
