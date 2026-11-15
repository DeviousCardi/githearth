//! Case U30c: the one outbound request client — the single-client guard.
//!
//! The client's behaviour (the pin, every refusal, the bounds, the deadline) is
//! proved by the unit tests in `src/outbound.rs`, where the injected transport
//! and resolver are reachable. This file proves the structural half that makes
//! those guarantees hold for the whole server:
//!
//! * every crate's `src` (and `benches`) is scanned for a socket, a DNS call or
//!   an HTTP client, and the only file exempt as a whole is `outbound.rs` — the
//!   client itself;
//! * every other exception is a line-level `// outbound-exempt: <reason>`
//!   marker, so the reason is visible where the connection is opened and a
//!   second connection in the same file is still refused;
//! * no proxy variable is read, so an egress proxy cannot move the connection
//!   away from the checked address;
//! * the transport is not public and the platform resolver is called in one
//!   place, so a module cannot construct a connection or resolve a name.

use githearth_server::{
    config::Config,
    outbound::{AddressClass, OutboundClient, OutboundError, OutboundRequest, classify_address},
};
use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// An outbound socket, a DNS lookup or an HTTP client. `outbound.rs` is the
/// client; a hit anywhere else must carry a line marker.
const RAW_PATTERNS: &[&str] = &[
    "TcpStream::connect",
    "TcpSocket",
    "UdpSocket",
    "socket2",
    "lookup_host",
    "to_socket_addrs",
    "ToSocketAddrs",
    "TcpTransport",
    ".round_trip(",
    "reqwest",
    "ureq",
    "isahc",
    "hyper::Client",
    "hyper_util",
    "Command::new(\"curl\")",
    "Command::new(\"wget\")",
];

/// A whole HTTP client library. These are refused everywhere, including tests:
/// a test that needs the server's API uses the server's routes, not a second
/// HTTP stack that could later be copied into production.
const HTTP_CLIENT_PATTERNS: &[&str] = &[
    "reqwest",
    "ureq",
    "isahc",
    "hyper::Client",
    "hyper_util",
    "Command::new(\"curl\")",
    "Command::new(\"wget\")",
];

const EXEMPTION: &str = "// outbound-exempt:";

fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//") || line.starts_with('*')
}

#[test]
fn only_the_outbound_module_opens_outbound_connections() {
    let workspace = workspace();
    let mut scanned = 0usize;
    let mut offenders = Vec::new();

    let crates = workspace.join("crates");
    let mut crate_dirs: Vec<PathBuf> = std::fs::read_dir(&crates)
        .expect("the workspace has a crates directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    crate_dirs.sort();

    for crate_dir in &crate_dirs {
        let name = crate_dir.file_name().unwrap().to_string_lossy().to_string();
        for (directory, patterns) in [
            (crate_dir.join("src"), RAW_PATTERNS),
            (crate_dir.join("benches"), RAW_PATTERNS),
            (crate_dir.join("tests"), HTTP_CLIENT_PATTERNS),
        ] {
            let mut files = Vec::new();
            rust_files(&directory, &mut files);
            for file in files {
                let file_name = file.file_name().unwrap().to_string_lossy().to_string();
                // The client itself is the one whole-file exemption.
                if name == "githearth-server" && file_name == "outbound.rs" {
                    continue;
                }
                // This guard names the patterns it looks for, so it cannot scan
                // itself without matching its own literal list.
                if file_name == "outbound_client.rs" {
                    continue;
                }
                scanned += 1;
                let text = std::fs::read_to_string(&file).unwrap();
                let lines: Vec<&str> = text.lines().collect();
                for (index, line) in lines.iter().enumerate() {
                    if is_comment(line) || line.contains(EXEMPTION) {
                        continue;
                    }
                    // The reason may sit on the line above the connection.
                    if index > 0 && lines[index - 1].contains(EXEMPTION) {
                        continue;
                    }
                    for pattern in patterns {
                        if line.contains(pattern) {
                            offenders.push(format!(
                                "{}:{}: {pattern}  ({line})",
                                file.display(),
                                index + 1
                            ));
                        }
                    }
                }
            }
        }
    }

    assert!(scanned > 100, "the scan found only {scanned} files");
    assert!(
        offenders.is_empty(),
        "outbound connections must go through outbound.rs; found:\n{}",
        offenders.join("\n")
    );

    // The one whole-file exemption must be the client. No other file may be
    // skipped, and the exception files must use line markers.
    let server_src = workspace.join("crates/githearth-server/src");
    let mail = std::fs::read_to_string(server_src.join("mail.rs")).unwrap();
    let maintenance = std::fs::read_to_string(server_src.join("maintenance.rs")).unwrap();
    assert_eq!(
        mail.matches(EXEMPTION).count(),
        1,
        "the SMTP exception must be one marked line"
    );
    assert_eq!(
        maintenance.matches(EXEMPTION).count(),
        2,
        "the maintenance probe and control request each need a marked line"
    );
    assert!(mail.contains("resolve_operator_destination"));
    assert!(mail.contains("connect_timeout(&address"));
}

#[test]
fn no_proxy_variable_can_move_the_pinned_connection() {
    let workspace = workspace();
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(workspace.join("crates"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
    {
        rust_files(&crate_dir.join("src"), &mut files);
        rust_files(&crate_dir.join("tests"), &mut files);
    }
    let mut offenders = Vec::new();
    for file in files {
        if file
            .file_name()
            .is_some_and(|name| name == "outbound_client.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            if text.contains(name) {
                offenders.push(format!("{}: {name}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the client does not support a proxy and must not read one; found:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_transport_is_private_and_resolves_in_exactly_one_place() {
    let source =
        std::fs::read_to_string(workspace().join("crates/githearth-server/src/outbound.rs"))
            .unwrap();
    assert!(
        source.contains("pub(crate) struct TcpTransport"),
        "the raw transport must not be reachable from another module or crate"
    );
    assert!(!source.contains("pub struct TcpTransport"));
    assert!(
        source.contains("pub(crate) trait HttpTransport"),
        "the injection seam must not be public"
    );
    assert_eq!(
        source.matches("lookup_host").count(),
        1,
        "the platform resolver is called in exactly one place"
    );
    assert_eq!(
        source.matches("to_socket_addrs").count(),
        1,
        "the SMTP destination resolves in exactly one checked helper"
    );
    assert!(
        source.contains("TcpStream::connect_timeout(&connection.address"),
        "the transport connects to the checked SocketAddr, never a name"
    );

    // Nothing else in the workspace calls the platform resolver.
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(workspace().join("crates"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
    {
        rust_files(&crate_dir.join("src"), &mut files);
    }
    for file in files {
        if file.file_name().is_some_and(|name| name == "outbound.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for pattern in ["lookup_host", "to_socket_addrs", "ToSocketAddrs"] {
            assert!(
                !text.contains(pattern),
                "{} resolves a name outside the checked client: {pattern}",
                file.display()
            );
        }
    }
}

/// The default installation may call nothing: an empty allowlist refuses the
/// request before it resolves, and the client is shareable across tasks.
#[tokio::test]
async fn a_default_client_refuses_everything_without_opening_a_socket() {
    let client = OutboundClient::from_config(&Config::default()).expect("the defaults load");
    let error = client
        .send(OutboundRequest::get("https://example.test/"))
        .await
        .expect_err("an empty allowlist calls nothing");
    assert!(matches!(error, OutboundError::NotAllowlisted { .. }));
    let record = client.records().last().cloned().unwrap();
    assert!(!record.allowed);
    assert!(record.reason.contains("allowlist"), "{record:?}");
}

#[test]
fn the_refusal_classification_is_the_public_contract() {
    assert_eq!(
        classify_address("169.254.169.254".parse().unwrap()),
        AddressClass::Metadata
    );
    assert_eq!(
        classify_address("127.0.0.1".parse().unwrap()),
        AddressClass::Loopback
    );
}
