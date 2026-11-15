//! The outbound request client (M4.2 increment 7D, case U30c; ADR 0014 §7).
//!
//! Every HTTP request the server itself makes — OIDC discovery, external action
//! sources, webhooks, mirrors, importers — goes through [`OutboundClient`]. It
//! is one seam with four rules, each enforced here rather than by the caller:
//!
//! 1. **Egress allowlist.** A destination is refused unless an allowlist entry
//!    names it. Link-local, loopback, private-range and cloud-metadata answers
//!    are refused as well, and the metadata endpoints have their own operator
//!    option so opening a private network never opens cloud credentials.
//! 2. **Redirects.** A redirect to a different host is always refused. A
//!    same-host redirect is followed up to [`Limits::max_redirects`], but it
//!    may not change the scheme and its port must stay allowlisted.
//! 3. **DNS pinning.** The hostname is resolved exactly once per hop, every
//!    answer is checked, and the chosen [`SocketAddr`] — not the name — is
//!    handed to the transport. A DNS answer that changes between the check and
//!    the connect cannot redirect the connection: the connection only ever
//!    receives an address that was checked.
//! 4. **Bounds and a record.** The whole exchange, including redirects and the
//!    body read, is bounded in time and bytes, and the one absolute deadline is
//!    handed to the blocking transport so it stops reading when the caller
//!    stops waiting. Every response line is read through a bounded reader.
//!    Every allow and every refusal is recorded with its reason, both in the
//!    tracing log and in the in-process ring returned by
//!    [`OutboundClient::records`].
//!
//! The transport and the resolver are injectable ([`HttpTransport`],
//! [`Resolver`]) exactly as `mail::MailTransport` is, so tests never open a
//! socket; both traits and the raw transport are crate-private, so no module
//! outside this one can construct a connection. `tests/outbound_client.rs`
//! enforces the single-client rule: it scans every crate for a socket, a DNS
//! call or an HTTP client and allows only this file as a whole, plus individual
//! `// outbound-exempt:` lines. SMTP (`mail.rs`) is not HTTP but resolves and
//! pins through [`resolve_operator_destination`], the same address policy; the
//! on-host maintenance control channel (`maintenance.rs`) is a documented
//! loopback-only exception recorded in ADR 0014 §7.

use crate::config::Config;
use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// How an IP address is treated by the egress policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressClass {
    Global,
    Metadata,
    Loopback,
    LinkLocal,
    Private,
    Unspecified,
    Multicast,
    Reserved,
}

impl AddressClass {
    fn refusal(self) -> &'static str {
        match self {
            Self::Global => "global address",
            Self::Metadata => "cloud metadata endpoint",
            Self::Loopback => "loopback address",
            Self::LinkLocal => "link-local address",
            Self::Private => "private-range address",
            Self::Unspecified => "unspecified address",
            Self::Multicast => "multicast address",
            Self::Reserved => "reserved address",
        }
    }
}

/// The cloud-metadata endpoints refused by name, not only by their range.
/// They are checked before the range classes, so `outbound_allow_private` does
/// not silently open them; only `outbound_allow_metadata` does.
const METADATA_V4: [Ipv4Addr; 5] = [
    Ipv4Addr::new(169, 254, 169, 254), // AWS, GCP, Azure, OpenStack IMDS
    Ipv4Addr::new(169, 254, 170, 2),   // AWS ECS task metadata
    Ipv4Addr::new(100, 100, 100, 200), // Alibaba Cloud
    Ipv4Addr::new(192, 0, 0, 192),     // Oracle Cloud
    Ipv4Addr::new(168, 63, 129, 16),   // Azure wire server
];
const METADATA_V6: [Ipv6Addr; 1] = [
    Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254), // AWS IMDS over IPv6
];

/// The classification of one resolved address.
///
/// An IPv6 transition form is classified by the IPv4 address it carries, not by
/// its IPv6 spelling: `::ffff:127.0.0.1` (mapped), `::127.0.0.1` and
/// `::ffff:0:127.0.0.1` (compatible and translated), `64:ff9b::a9fe:a9fe`
/// (NAT64) and `2002:7f00:1::` (6to4) are all loopback for this policy. The
/// transition prefix cannot be used to smuggle a refused address past the
/// check.
pub fn classify_address(address: IpAddr) -> AddressClass {
    let address = match address {
        IpAddr::V6(v6) => embedded_ipv4(v6).map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    };
    match address {
        IpAddr::V4(v4) if METADATA_V4.contains(&v4) => AddressClass::Metadata,
        IpAddr::V6(v6) if METADATA_V6.contains(&v6) => AddressClass::Metadata,
        IpAddr::V4(v4) if v4.is_loopback() => AddressClass::Loopback,
        IpAddr::V6(v6) if v6.is_loopback() => AddressClass::Loopback,
        IpAddr::V4(v4) if v4.is_link_local() => AddressClass::LinkLocal,
        IpAddr::V6(v6) if v6.is_unicast_link_local() => AddressClass::LinkLocal,
        IpAddr::V4(v4) if v4.is_private() || is_shared_v4(v4) => AddressClass::Private,
        IpAddr::V6(v6) if is_unique_local(v6) => AddressClass::Private,
        // 64:ff9b:1::/48 is the local-use NAT64 prefix (RFC 8215); its IPv4
        // bits are not in a fixed position, so the whole prefix is refused.
        IpAddr::V6(v6) if is_local_nat64(v6) => AddressClass::Reserved,
        IpAddr::V4(v4) if v4.is_unspecified() => AddressClass::Unspecified,
        IpAddr::V6(v6) if v6.is_unspecified() => AddressClass::Unspecified,
        IpAddr::V4(v4) if v4.is_multicast() => AddressClass::Multicast,
        IpAddr::V6(v6) if v6.is_multicast() => AddressClass::Multicast,
        IpAddr::V4(v4)
            if v4.is_broadcast()
                || v4.is_documentation()
                || is_benchmark_v4(v4)
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 240 =>
        {
            AddressClass::Reserved
        }
        IpAddr::V6(v6) if is_documentation_v6(v6) => AddressClass::Reserved,
        _ => AddressClass::Global,
    }
}

/// The IPv4 address an IPv6 transition form carries, if any. This is the only
/// place a spelling changes which range check applies.
fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = address.to_ipv4_mapped() {
        return Some(v4);
    }
    let segments = address.segments();
    // `::/96` IPv4-compatible (deprecated). `::` and `::1` keep their own
    // meaning; anything else carries the low 32 bits.
    if segments[..6].iter().all(|segment| *segment == 0) && !(segments[6] == 0 && segments[7] <= 1)
    {
        return Some(low_v4(&segments));
    }
    // `::ffff:0:0/96` IPv4-translated (RFC 2765).
    if segments[..4].iter().all(|segment| *segment == 0)
        && segments[4] == 0xffff
        && segments[5] == 0
    {
        return Some(low_v4(&segments));
    }
    // `64:ff9b::/96` NAT64 (RFC 6052): the low 32 bits are the IPv4 address.
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|segment| *segment == 0)
    {
        return Some(low_v4(&segments));
    }
    // `2002::/16` 6to4 (RFC 3056): segments 1 and 2 are the IPv4 address.
    if segments[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        ));
    }
    None
}

/// The IPv4 address in the low 32 bits of an IPv6 address.
fn low_v4(segments: &[u16; 8]) -> Ipv4Addr {
    Ipv4Addr::new(
        (segments[6] >> 8) as u8,
        segments[6] as u8,
        (segments[7] >> 8) as u8,
        segments[7] as u8,
    )
}

/// `198.18.0.0/15`, RFC 2544 benchmarking.
fn is_benchmark_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 198 && (18..=19).contains(&octets[1])
}

/// `64:ff9b:1::/48`, the local-use NAT64 prefix.
fn is_local_nat64(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001
}

/// `100.64.0.0/10`, carrier-grade NAT. `Ipv4Addr::is_private` does not cover it
/// and it is not routable on the public internet.
fn is_shared_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

/// `fc00::/7`, unique local addresses.
fn is_unique_local(address: Ipv6Addr) -> bool {
    (address.segments()[0] & 0xfe00) == 0xfc00
}

/// `2001:db8::/32`, documentation. Stable as arithmetic rather than the
/// unstable `Ipv6Addr::is_documentation`.
fn is_documentation_v6(address: Ipv6Addr) -> bool {
    address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8
}

/// One allowlist entry: an exact host or a `*.` subdomain wildcard, with an
/// optional port. Parsing is shared with `Config::validate`, so an entry that
/// loads is an entry this policy understands.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AllowEntry {
    host: String,
    wildcard: bool,
    port: Option<u16>,
}

impl AllowEntry {
    fn parse(entry: &str) -> Result<Self, String> {
        if entry.is_empty() || entry.len() > 260 {
            return Err("entry must be 1..=260 bytes".into());
        }
        if entry
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(format!(
                "entry \"{entry}\" contains whitespace or control bytes"
            ));
        }
        if entry.contains("://") || entry.contains('/') || entry.contains('@') {
            return Err(format!(
                "entry \"{entry}\" must be a host or host:port, not a URL"
            ));
        }
        let (wildcard, rest) = match entry.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, entry),
        };
        if rest.contains('*') {
            return Err(format!(
                "entry \"{entry}\" may only use a leading \"*.\" wildcard"
            ));
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| format!("entry \"{entry}\" has an invalid port"))?;
                if port == 0 {
                    return Err(format!("entry \"{entry}\" has an invalid port"));
                }
                (host, Some(port))
            }
            None => (rest, None),
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        if wildcard && host.is_empty() {
            return Err(format!("entry \"{entry}\" has an empty host"));
        }
        // An IP-literal entry can never match: every destination is a DNS name
        // (`Destination::parse` refuses IP literals), so accepting one here
        // would be config that silently never applies.
        if host.parse::<IpAddr>().is_ok() || is_legacy_ipv4_literal(host) {
            return Err(format!(
                "entry \"{entry}\" is an IP address; use the destination's DNS name"
            ));
        }
        // A wildcard needs a registrable domain under it. `*.com` would make
        // every `.com` host callable in one line.
        if wildcard && host.split('.').count() < 2 {
            return Err(format!(
                "entry \"{entry}\" needs at least two labels under the wildcard"
            ));
        }
        if !valid_hostname(host) {
            return Err(format!("entry \"{entry}\" has an invalid host"));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            wildcard,
            port,
        })
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        match self.port {
            // A host-only entry means the scheme's default ports, not every
            // port: `api.example.test` must not open `api.example.test:22`.
            None if !matches!(port, 80 | 443) => return false,
            Some(allowed) if allowed != port => return false,
            _ => {}
        }
        if self.wildcard {
            host.ends_with(&format!(".{}", self.host))
        } else {
            host == self.host
        }
    }
}

/// Validates one allowlist entry. `Config::validate` calls this so an operator
/// typo is refused at load rather than silently never matching.
pub fn validate_allowlist_entry(entry: &str) -> Result<(), String> {
    AllowEntry::parse(entry).map(|_| ())
}

fn valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// True for a host spelled as a legacy numeric IPv4 address — `127.1`,
/// `2130706433`, `0x7f.1`, `0300.0250.1.1`. `IpAddr::from_str` rejects these
/// (and accepts only the dotted-quad form), but `getaddrinfo` still resolves
/// them, so a destination that reaches the resolver would become an address the
/// policy never saw. Refusing the spelling here keeps "every destination is a
/// DNS name" true by construction rather than by resolver behaviour.
fn is_legacy_ipv4_literal(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return false;
    }
    parts.iter().all(|part| {
        if part.is_empty() {
            return false;
        }
        if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
            return !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
        }
        part.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// The egress policy. It is immutable once built; configuration is read once at
/// start-up, so a running request cannot be raced by an operator edit.
#[derive(Clone, Debug)]
pub struct EgressPolicy {
    entries: Vec<AllowEntry>,
    allow_private: bool,
    allow_metadata: bool,
}

impl EgressPolicy {
    pub fn new(
        entries: &[String],
        allow_private: bool,
        allow_metadata: bool,
    ) -> Result<Self, String> {
        let entries = entries
            .iter()
            .map(|entry| AllowEntry::parse(entry))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            entries,
            allow_private,
            allow_metadata,
        })
    }

    pub fn allow_private(&self) -> bool {
        self.allow_private
    }

    pub fn allow_metadata(&self) -> bool {
        self.allow_metadata
    }

    /// `Some(reason)` when the destination may be called, `None` when it is not
    /// on the allowlist.
    fn permits(&self, host: &str, port: u16) -> Option<String> {
        let host = host.to_ascii_lowercase();
        let exact = self
            .entries
            .iter()
            .find(|entry| !entry.wildcard && entry.matches(&host, port));
        if let Some(entry) = exact {
            return Some(if entry.port.is_some() {
                format!("allowlisted host and port {port}")
            } else {
                format!("allowlisted host on standard port {port}")
            });
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.wildcard && entry.matches(&host, port))
        {
            return Some(format!("allowlisted subdomain on port {port}"));
        }
        None
    }

    /// Checks one resolved answer against the non-global and metadata rules.
    /// `Err` carries the reason recorded for the refusal.
    fn permits_address(&self, address: IpAddr) -> Result<&'static str, String> {
        match classify_address(address) {
            AddressClass::Global => Ok("global address"),
            AddressClass::Metadata if self.allow_metadata => {
                Ok("cloud metadata endpoint permitted by outbound_allow_metadata")
            }
            AddressClass::Metadata => Err(format!(
                "cloud metadata endpoint {address} refused; \
                 set outbound_allow_metadata to permit it"
            )),
            class if self.allow_private => Ok(match class {
                AddressClass::Global => "global address",
                _ => "non-global address permitted by outbound_allow_private",
            }),
            class => Err(format!(
                "{} {address} refused; set outbound_allow_private to permit it",
                class.refusal()
            )),
        }
    }
}

/// Resolves an operator-configured, non-HTTP destination (the SMTP relay) once
/// with the platform resolver, checks **every** answer with the same
/// [`classify_address`] policy, and returns one pinned [`SocketAddr`] to connect
/// to. The caller connects to the returned address directly, so there is no
/// second resolution and no name for the socket layer to resolve.
///
/// Cloud-metadata answers are always refused. Loopback, link-local and
/// private-range answers need `outbound_allow_private`, the same option that
/// opens them for HTTP: one installation-wide egress policy, not two.
pub(crate) fn resolve_operator_destination(
    host: &str,
    port: u16,
    allow_private: bool,
) -> Result<SocketAddr, String> {
    let policy = EgressPolicy::new(&[], allow_private, false)?;
    let answers: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| format!("cannot resolve {host}"))?
        .collect();
    choose_operator_address(host, answers, &policy)
}

fn choose_operator_address(
    host: &str,
    answers: Vec<SocketAddr>,
    policy: &EgressPolicy,
) -> Result<SocketAddr, String> {
    let mut chosen = None;
    for address in answers {
        match policy.permits_address(address.ip()) {
            Ok(_) if chosen.is_none() => chosen = Some(address),
            Ok(_) => {}
            Err(reason) => return Err(format!("{host}: {reason}")),
        }
    }
    chosen.ok_or_else(|| format!("{host} resolved to no usable address"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundRecord {
    pub host: String,
    pub address: Option<IpAddr>,
    pub allowed: bool,
    pub reason: String,
}

/// The bounded request/response limits. `Config` supplies them in seconds and
/// bytes; tests build a [`Limits`] directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub max_request_bytes: usize,
    pub max_redirects: usize,
}

impl Limits {
    /// The defaults published in `githearth.example.toml` (ADR 0014 §7):
    /// 30 s for the whole exchange, 5 MiB per response, 1 MiB per request and
    /// at most 5 redirects.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
    pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 5_242_880;
    pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1_048_576;
    pub const DEFAULT_MAX_REDIRECTS: usize = 5;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Self::DEFAULT_TIMEOUT,
            max_response_bytes: Self::DEFAULT_MAX_RESPONSE_BYTES,
            max_request_bytes: Self::DEFAULT_MAX_REQUEST_BYTES,
            max_redirects: Self::DEFAULT_MAX_REDIRECTS,
        }
    }
}

/// A request a feature asks the client to make. Headers are copied into the
/// wire request except `Host`, `Content-Length`, `Connection`,
/// `Transfer-Encoding`, `TE`, `Trailer`, `Upgrade` and `Accept-Encoding`, which
/// the client owns so a caller cannot break framing or ask for an encoding the
/// transport cannot decode.
pub struct OutboundRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl OutboundRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn post(url: impl Into<String>, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: vec![("content-type".into(), content_type.into())],
            body,
        }
    }
}

/// A bounded response. `final_url` is the URL that produced it, after
/// same-host redirects; it is the origin the caller may trust for a relative
/// link, not a new destination.
pub struct OutboundResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub final_url: String,
    pub hops: usize,
}

impl OutboundResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for OutboundResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header values and the body can carry a token from an upstream
        // service; a debug line names neither.
        f.debug_struct("OutboundResponse")
            .field("status", &self.status)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.len())
            .field("final_url", &self.final_url)
            .field("hops", &self.hops)
            .finish()
    }
}

/// A refusal or transport failure. The text is fixed or built from the host and
/// resolved address only; request headers, bodies and query strings never reach
/// an error, a log line or a stored reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutboundError {
    InvalidRequest(String),
    NotAllowlisted {
        host: String,
        port: u16,
    },
    ResolutionFailed {
        host: String,
    },
    AddressRefused {
        host: String,
        address: IpAddr,
        class: AddressClass,
    },
    RedirectToDifferentHost {
        from: String,
        to: String,
    },
    RedirectSchemeChanged {
        from: String,
        to: String,
    },
    RedirectLoop,
    TooManyRedirects {
        max: usize,
    },
    ResponseTooLarge {
        limit: usize,
    },
    RequestTooLarge {
        limit: usize,
    },
    Timeout {
        millis: u64,
    },
    Transport,
}

impl fmt::Display for OutboundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(reason) => write!(f, "invalid outbound request: {reason}"),
            Self::NotAllowlisted { host, port } => {
                write!(
                    f,
                    "destination {host}:{port} is not on the egress allowlist"
                )
            }
            Self::ResolutionFailed { host } => write!(f, "cannot resolve outbound host {host}"),
            Self::AddressRefused { address, class, .. } => {
                write!(f, "refused {} address {address}", class.refusal())
            }
            Self::RedirectToDifferentHost { from, to } => {
                write!(f, "refused redirect from {from} to a different host {to}")
            }
            Self::RedirectSchemeChanged { from, to } => write!(
                f,
                "refused redirect from {from} to {to}: the scheme may not change"
            ),
            Self::RedirectLoop => f.write_str("refused redirect loop"),
            Self::TooManyRedirects { max } => {
                write!(f, "refused more than {max} outbound redirects")
            }
            Self::ResponseTooLarge { limit } => {
                write!(f, "outbound response exceeded {limit} bytes")
            }
            Self::RequestTooLarge { limit } => {
                write!(f, "outbound request exceeded {limit} bytes")
            }
            Self::Timeout { millis } => {
                write!(f, "outbound request timed out after {millis}ms")
            }
            Self::Transport => f.write_str("outbound transport failed"),
        }
    }
}

impl std::error::Error for OutboundError {}

/// The connection the transport is allowed to make: an address that was already
/// resolved and checked. There is deliberately no hostname to re-resolve.
///
/// Crate-private on purpose: the only way to obtain one is
/// [`OutboundClient::send`], so no module can call the transport with an
/// unchecked address. Integration tests use the client's public surface; the
/// pin and bounds are proved by the unit tests in this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Connection {
    pub(crate) address: SocketAddr,
    pub(crate) server_name: String,
    pub(crate) tls: bool,
}

/// The HTTP/1.1 request the transport writes. Its `target` is path and query.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WireRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) host_header: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

/// A response the transport read. The client re-checks `body.len()` against the
/// limit so an injected transport cannot bypass the bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WireResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportError {
    Connect,
    Tls,
    Protocol,
    TooLarge,
    Timeout,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect => "cannot connect to the checked address",
            Self::Tls => "TLS negotiation failed",
            Self::Protocol => "malformed HTTP response",
            Self::TooLarge => "response exceeded the size bound",
            Self::Timeout => "the request deadline expired in the transport",
        })
    }
}

pub(crate) type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WireResponse, TransportError>> + Send + 'a>>;

/// The injection seam, mirroring `MailTransport`. A test implementation records
/// the [`Connection`] it was given and returns canned responses.
pub(crate) trait HttpTransport: Send + Sync {
    fn round_trip<'a>(
        &'a self,
        connection: &'a Connection,
        request: &'a WireRequest,
        deadline: Instant,
        max_response_bytes: usize,
    ) -> TransportFuture<'a>;
}

pub(crate) type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, String>> + Send + 'a>>;

/// The injection seam for DNS. Tests drive a fixed, changing sequence of
/// answers; the production resolver uses the platform resolver once per hop.
pub(crate) trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a>;
}

/// The platform resolver. It is called once per hop and the resulting addresses
/// are checked and pinned; the transport never sees the name.
pub(crate) struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
        Box::pin(async move {
            let answers = tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| "DNS resolution failed".to_owned())?;
            let mut addresses = Vec::new();
            for answer in answers {
                if !addresses.contains(&answer.ip()) {
                    addresses.push(answer.ip());
                }
            }
            Ok(addresses)
        })
    }
}

/// The parsed destination. Only `http` and `https` are accepted, userinfo is
/// refused, and an IP literal is refused: every destination is a DNS name, so
/// the resolution that is pinned is always the one the policy checked.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Destination {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

impl Destination {
    fn parse(url: &str) -> Result<Self, OutboundError> {
        if url.len() > 8192 {
            return Err(OutboundError::InvalidRequest("URL is too long".into()));
        }
        if url.bytes().any(|b| b.is_ascii_control() || b == b' ') {
            return Err(OutboundError::InvalidRequest(
                "URL contains a control byte or space".into(),
            ));
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| OutboundError::InvalidRequest("URL has no scheme".into()))?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            _ => {
                return Err(OutboundError::InvalidRequest(
                    "URL scheme must be http or https".into(),
                ));
            }
        };
        let rest = rest.split('#').next().unwrap_or(rest);
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(OutboundError::InvalidRequest("URL has no host".into()));
        }
        if authority.contains('@') {
            return Err(OutboundError::InvalidRequest(
                "URL userinfo is refused".into(),
            ));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| OutboundError::InvalidRequest("URL has an invalid port".into()))?;
                if port == 0 {
                    return Err(OutboundError::InvalidRequest(
                        "URL has an invalid port".into(),
                    ));
                }
                (host, port)
            }
            None => (authority, scheme.default_port()),
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        if host.parse::<IpAddr>().is_ok() || is_legacy_ipv4_literal(host) {
            return Err(OutboundError::InvalidRequest(
                "an IP-literal destination is refused; use a DNS name".into(),
            ));
        }
        if !valid_hostname(host) {
            return Err(OutboundError::InvalidRequest(
                "URL host is not a valid DNS name".into(),
            ));
        }
        if !path.starts_with('/') {
            return Err(OutboundError::InvalidRequest(
                "URL path is malformed".into(),
            ));
        }
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path: path.to_owned(),
        })
    }

    fn authority(&self) -> String {
        if self.port == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}{}",
            self.scheme.as_str(),
            self.authority(),
            self.path
        )
    }
}

/// Resolves a `Location` against the current destination. Fragments are
/// dropped; only `http`/`https` absolute forms, protocol-relative forms and
/// path forms are accepted.
fn resolve_location(current: &Destination, location: &str) -> Result<Destination, OutboundError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(OutboundError::InvalidRequest(
            "redirect has an empty Location".into(),
        ));
    }
    let location = location.split('#').next().unwrap_or(location);
    // A server may write the scheme in any case; compare without case so
    // `HTTPS://host/` is an absolute destination, not a relative path that
    // would be fetched from the allowed host.
    let scheme_case = location.to_ascii_lowercase();
    if scheme_case.starts_with("http://") || scheme_case.starts_with("https://") {
        return Destination::parse(location);
    }
    if let Some(rest) = location.strip_prefix("//") {
        return Destination::parse(&format!("{}://{rest}", current.scheme.as_str()));
    }
    if location.starts_with('/') {
        let mut next = current.clone();
        next.path = location.to_owned();
        return Ok(next);
    }
    let base = current
        .path
        .rsplit_once('/')
        .map(|(head, _)| head)
        .unwrap_or("");
    let mut next = current.clone();
    next.path = format!("{base}/{location}");
    Ok(next)
}

/// The one outbound client. It is cheap to share (`Arc`) and safe to call from
/// any task; the record ring is the only mutable state.
pub struct OutboundClient {
    policy: EgressPolicy,
    limits: Limits,
    transport: Arc<dyn HttpTransport>,
    resolver: Arc<dyn Resolver>,
    records: Mutex<VecDeque<OutboundRecord>>,
}

/// How many decisions the in-process ring keeps. The tracing log is the durable
/// copy; the ring exists for `doctor`, tests and an operator inspection.
const RECORD_CAPACITY: usize = 512;

impl OutboundClient {
    /// Builds a client around the given policy and seams. Crate-private: the
    /// transport and resolver cannot be supplied from outside this crate, so
    /// no other module can bypass the policy by constructing a connection.
    pub(crate) fn new(
        policy: EgressPolicy,
        limits: Limits,
        transport: Arc<dyn HttpTransport>,
        resolver: Arc<dyn Resolver>,
    ) -> Self {
        Self {
            policy,
            limits,
            transport,
            resolver,
            records: Mutex::new(VecDeque::with_capacity(RECORD_CAPACITY)),
        }
    }

    /// Builds the production client: the platform resolver and the TCP/TLS
    /// transport. `Config::validate` has already checked the allowlist.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let policy = EgressPolicy::new(
            &config.outbound_allowlist,
            config.outbound_allow_private,
            config.outbound_allow_metadata,
        )
        .map_err(|reason| anyhow::anyhow!("outbound_allowlist: {reason}"))?;
        Ok(Self::new(
            policy,
            Limits {
                timeout: Duration::from_secs(config.outbound_timeout_seconds),
                max_response_bytes: config.outbound_max_response_bytes,
                max_request_bytes: config.outbound_max_request_bytes,
                max_redirects: config.outbound_max_redirects,
            },
            Arc::new(TcpTransport),
            Arc::new(SystemResolver),
        ))
    }

    pub fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The recorded decisions, oldest first.
    pub fn records(&self) -> Vec<OutboundRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    pub fn clear_records(&self) {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn record(&self, record: OutboundRecord) {
        if record.allowed {
            tracing::info!(
                target: "outbound",
                host = %record.host,
                address = record.address.map(|a| a.to_string()).unwrap_or_default(),
                reason = %record.reason,
                "outbound request allowed"
            );
        } else {
            tracing::warn!(
                target: "outbound",
                host = %record.host,
                address = record.address.map(|a| a.to_string()).unwrap_or_default(),
                reason = %record.reason,
                "outbound request refused"
            );
        }
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if records.len() == RECORD_CAPACITY {
            records.pop_front();
        }
        records.push_back(record);
    }

    fn record_refusal(&self, host: &str, address: Option<IpAddr>, reason: impl Into<String>) {
        self.record(OutboundRecord {
            host: host.to_owned(),
            address,
            allowed: false,
            reason: reason.into(),
        });
    }

    /// Sends one request through the policy, the pinned connection and the
    /// bounds. One deadline covers resolution, every redirect hop and the body
    /// read, and the same absolute deadline is handed to the transport, so the
    /// blocking reader stops when the caller stops waiting.
    pub async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, OutboundError> {
        let host = Destination::parse(&request.url)
            .map(|destination| destination.host)
            .unwrap_or_else(|_| "<invalid>".into());
        let timeout = self.limits.timeout;
        let deadline = Instant::now() + timeout;
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.send_inner(request, deadline),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                self.record_refusal(&host, None, format!("request timed out after {millis}ms"));
                Err(OutboundError::Timeout { millis })
            }
        }
    }

    async fn send_inner(
        &self,
        request: OutboundRequest,
        deadline: Instant,
    ) -> Result<OutboundResponse, OutboundError> {
        let OutboundRequest {
            method,
            url,
            headers,
            body,
        } = request;
        let mut destination = match Destination::parse(&url) {
            Ok(destination) => destination,
            Err(error) => {
                self.record_refusal("<invalid>", None, format!("request refused: {error}"));
                return Err(error);
            }
        };
        let mut method = match validate_method(&method) {
            Ok(method) => method,
            Err(error) => {
                self.record_refusal(&destination.host, None, format!("request refused: {error}"));
                return Err(error);
            }
        };
        if body.len() > self.limits.max_request_bytes {
            self.record_refusal(
                &destination.host,
                None,
                format!(
                    "request body of {} bytes exceeded the {} byte bound",
                    body.len(),
                    self.limits.max_request_bytes
                ),
            );
            return Err(OutboundError::RequestTooLarge {
                limit: self.limits.max_request_bytes,
            });
        }
        let mut headers = match sanitize_headers(&headers) {
            Ok(headers) => headers,
            Err(error) => {
                self.record_refusal(&destination.host, None, format!("request refused: {error}"));
                return Err(error);
            }
        };
        let mut body = body;
        let mut seen: Vec<Destination> = Vec::new();

        for hop in 0..=self.limits.max_redirects {
            let allow_reason = match self.policy.permits(&destination.host, destination.port) {
                Some(reason) => reason,
                None => {
                    self.record_refusal(
                        &destination.host,
                        None,
                        "destination is not on the egress allowlist",
                    );
                    return Err(OutboundError::NotAllowlisted {
                        host: destination.host.clone(),
                        port: destination.port,
                    });
                }
            };

            // One resolution per hop, then every answer is checked. If any
            // answer is forbidden the whole resolution is refused: a
            // round-robin host cannot smuggle one bad address past a check that
            // looked only at the first.
            let addresses = match self
                .resolver
                .resolve(&destination.host, destination.port)
                .await
            {
                Ok(addresses) if !addresses.is_empty() => addresses,
                _ => {
                    self.record_refusal(
                        &destination.host,
                        None,
                        "DNS resolution returned no usable address",
                    );
                    return Err(OutboundError::ResolutionFailed {
                        host: destination.host.clone(),
                    });
                }
            };
            let mut pinned: Option<(IpAddr, &'static str)> = None;
            for address in &addresses {
                match self.policy.permits_address(*address) {
                    Ok(reason) if pinned.is_none() => pinned = Some((*address, reason)),
                    Ok(_) => {}
                    Err(reason) => {
                        let class = classify_address(*address);
                        self.record_refusal(&destination.host, Some(*address), reason);
                        return Err(OutboundError::AddressRefused {
                            host: destination.host.clone(),
                            address: *address,
                            class,
                        });
                    }
                }
            }
            let Some((pinned, address_reason)) = pinned else {
                self.record_refusal(
                    &destination.host,
                    None,
                    "DNS resolution returned no usable address",
                );
                return Err(OutboundError::ResolutionFailed {
                    host: destination.host.clone(),
                });
            };

            // The allow is recorded before the connect, naming the exact
            // address the connect will use. This is the pin: the transport is
            // handed `pinned`, never the name, and cannot resolve it again.
            self.record(OutboundRecord {
                host: destination.host.clone(),
                address: Some(pinned),
                allowed: true,
                reason: format!("{allow_reason}; {address_reason}; DNS answer pinned to {pinned}"),
            });

            let connection = Connection {
                address: SocketAddr::new(pinned, destination.port),
                server_name: destination.host.clone(),
                tls: destination.scheme == Scheme::Https,
            };
            let wire = WireRequest {
                method: method.clone(),
                target: destination.path.clone(),
                host_header: destination.authority(),
                headers: headers.clone(),
                body: body.clone(),
            };
            let response = match self
                .transport
                .round_trip(&connection, &wire, deadline, self.limits.max_response_bytes)
                .await
            {
                Ok(response) => response,
                Err(TransportError::Timeout) => {
                    let millis = u64::try_from(self.limits.timeout.as_millis()).unwrap_or(u64::MAX);
                    self.record_refusal(
                        &destination.host,
                        Some(pinned),
                        format!("request timed out after {millis}ms in the transport"),
                    );
                    return Err(OutboundError::Timeout { millis });
                }
                Err(error) => {
                    self.record_refusal(
                        &destination.host,
                        Some(pinned),
                        format!("transport failure: {error}"),
                    );
                    return Err(OutboundError::Transport);
                }
            };

            if response.body.len() > self.limits.max_response_bytes {
                self.record_refusal(
                    &destination.host,
                    Some(pinned),
                    format!(
                        "response body of {} bytes exceeded the {} byte bound",
                        response.body.len(),
                        self.limits.max_response_bytes
                    ),
                );
                return Err(OutboundError::ResponseTooLarge {
                    limit: self.limits.max_response_bytes,
                });
            }

            if is_redirect(response.status) {
                let Some(location) = response
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                    .map(|(_, value)| value.clone())
                else {
                    self.record_refusal(
                        &destination.host,
                        Some(pinned),
                        "redirect response has no Location header",
                    );
                    return Err(OutboundError::InvalidRequest(
                        "redirect has no Location header".into(),
                    ));
                };
                let next = match resolve_location(&destination, &location) {
                    Ok(next) => next,
                    Err(error) => {
                        self.record_refusal(
                            &destination.host,
                            Some(pinned),
                            format!("redirect target is invalid: {error}"),
                        );
                        return Err(error);
                    }
                };
                if next.host != destination.host {
                    self.record_refusal(
                        &next.host,
                        None,
                        format!(
                            "redirect from {} to a different host {} refused",
                            destination.host, next.host
                        ),
                    );
                    return Err(OutboundError::RedirectToDifferentHost {
                        from: destination.host.clone(),
                        to: next.host.clone(),
                    });
                }
                if next.scheme != destination.scheme {
                    self.record_refusal(
                        &next.host,
                        Some(pinned),
                        format!(
                            "redirect from {} to {} refused: the scheme may not change",
                            destination.scheme.as_str(),
                            next.scheme.as_str()
                        ),
                    );
                    return Err(OutboundError::RedirectSchemeChanged {
                        from: destination.scheme.as_str().to_owned(),
                        to: next.scheme.as_str().to_owned(),
                    });
                }
                if self.policy.permits(&next.host, next.port).is_none() {
                    self.record_refusal(
                        &next.host,
                        None,
                        "redirect target port is not on the egress allowlist",
                    );
                    return Err(OutboundError::NotAllowlisted {
                        host: next.host.clone(),
                        port: next.port,
                    });
                }
                if seen.contains(&next) {
                    self.record_refusal(&next.host, Some(pinned), "redirect loop refused");
                    return Err(OutboundError::RedirectLoop);
                }
                seen.push(destination.clone());

                // 301/302 have historically been followed with a GET by
                // clients; 303 is explicitly a GET. 307/308 preserve the method
                // and body. Following the safer historical rule keeps a
                // redirected POST from re-sending a body to a same-host path.
                if response.status == 303
                    || (matches!(response.status, 301 | 302) && method != "GET" && method != "HEAD")
                {
                    method = "GET".into();
                    body.clear();
                    headers.retain(|(name, _)| {
                        !name.eq_ignore_ascii_case("content-type")
                            && !name.eq_ignore_ascii_case("content-length")
                    });
                }
                destination = next;
                continue;
            }

            return Ok(OutboundResponse {
                status: response.status,
                headers: response.headers,
                body: response.body,
                final_url: destination.to_string(),
                hops: hop + 1,
            });
        }

        self.record_refusal(
            &destination.host,
            None,
            format!("more than {} redirects refused", self.limits.max_redirects),
        );
        Err(OutboundError::TooManyRedirects {
            max: self.limits.max_redirects,
        })
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn validate_method(method: &str) -> Result<String, OutboundError> {
    let method = method.to_ascii_uppercase();
    if matches!(
        method.as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
    ) {
        Ok(method)
    } else {
        Err(OutboundError::InvalidRequest(
            "method must be GET, HEAD, POST, PUT, PATCH, DELETE or OPTIONS".into(),
        ))
    }
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Drops the framing and encoding headers the client owns, and refuses a header
/// that could split the request. A caller's `Host` cannot name a different
/// destination than the one that was checked.
fn sanitize_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, OutboundError> {
    let mut sanitized = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if !valid_header_name(name) {
            return Err(OutboundError::InvalidRequest(
                "request header name is invalid".into(),
            ));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(OutboundError::InvalidRequest(
                "request header value contains a line break".into(),
            ));
        }
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "accept-encoding"
        ) {
            continue;
        }
        sanitized.push((name.clone(), value.trim().to_owned()));
    }
    Ok(sanitized)
}

/// The production transport: a blocking HTTP/1.1 exchange on the blocking pool,
/// with TLS verified against the public roots. It only ever receives a
/// [`SocketAddr`], so it cannot re-resolve a name and defeat the pin. It is
/// crate-private; [`OutboundClient::send`] is the only way to make one.
pub(crate) struct TcpTransport;

impl HttpTransport for TcpTransport {
    fn round_trip<'a>(
        &'a self,
        connection: &'a Connection,
        request: &'a WireRequest,
        deadline: Instant,
        max_response_bytes: usize,
    ) -> TransportFuture<'a> {
        let connection = connection.clone();
        let request = request.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                round_trip_blocking(&connection, &request, deadline, max_response_bytes)
            })
            .await
            .map_err(|_| TransportError::Protocol)?
        })
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// The time left before the one request deadline. A zero remainder is the
/// deadline: no further read or write is started.
fn remaining(deadline: Instant) -> Result<Duration, TransportError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(TransportError::Timeout)
    } else {
        Ok(remaining)
    }
}

/// An I/O failure inside the transport. A socket timeout is the request's
/// deadline, not a malformed response, so the caller records it as a timeout.
fn io_transport(error: std::io::Error) -> TransportError {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => TransportError::Timeout,
        _ => TransportError::Protocol,
    }
}

/// A reader that resets the socket's read timeout to the time left before
/// **every** read. One timeout set once per connection is not a bound: a peer
/// that sends one byte every `timeout - 1s` keeps the reader alive forever.
struct DeadlineReader<R> {
    inner: R,
    socket: TcpStream,
    deadline: Instant,
}

impl<R: Read> Read for DeadlineReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = remaining(self.deadline)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "outbound deadline"))?;
        self.socket.set_read_timeout(Some(remaining)).ok();
        self.inner.read(buffer)
    }
}

fn round_trip_blocking(
    connection: &Connection,
    request: &WireRequest,
    deadline: Instant,
    max_response_bytes: usize,
) -> Result<WireResponse, TransportError> {
    let tcp =
        TcpStream::connect_timeout(&connection.address, remaining(deadline)?).map_err(|error| {
            match error.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    TransportError::Timeout
                }
                _ => TransportError::Connect,
            }
        })?;
    let socket = tcp.try_clone().map_err(|_| TransportError::Connect)?;
    let mut stream: Box<dyn ReadWrite> = if connection.tls {
        Box::new(tls_stream(tcp, &connection.server_name, deadline)?)
    } else {
        Box::new(tcp)
    };

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: githearth-outbound/1\r\n\
         Accept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        request.method, request.target, request.host_header
    );
    for (name, value) in &request.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", request.body.len()));
    socket
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|_| TransportError::Timeout)?;
    stream.write_all(head.as_bytes()).map_err(io_transport)?;
    if !request.body.is_empty() {
        socket
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| TransportError::Timeout)?;
        stream.write_all(&request.body).map_err(io_transport)?;
    }
    socket
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|_| TransportError::Timeout)?;
    stream.flush().map_err(io_transport)?;

    let mut reader = BufReader::new(DeadlineReader {
        inner: stream,
        socket,
        deadline,
    });
    read_response(&mut reader, &request.method, max_response_bytes)
}

/// The TLS client configuration. ALPN is deliberately empty: the transport
/// speaks HTTP/1.1 only, so there is no HTTP/2 connection to reuse and one
/// request is one connection. Adding `h2` here would let a peer multiplex, and
/// the test below fails if that happens.
fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn tls_stream(
    mut tcp: TcpStream,
    host: &str,
    deadline: Instant,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, TransportError> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| TransportError::Tls)?;
    let mut connection = rustls::ClientConnection::new(tls_config(), server_name)
        .map_err(|_| TransportError::Tls)?;
    // The handshake is part of the request's budget too.
    while connection.is_handshaking() {
        tcp.set_read_timeout(Some(remaining(deadline)?))
            .map_err(|_| TransportError::Timeout)?;
        tcp.set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| TransportError::Timeout)?;
        connection.complete_io(&mut tcp).map_err(io_transport)?;
    }
    Ok(rustls::StreamOwned::new(connection, tcp))
}

fn read_response<R: Read>(
    reader: &mut BufReader<R>,
    method: &str,
    max_response_bytes: usize,
) -> Result<WireResponse, TransportError> {
    // Skip up to five interim 1xx responses; they carry headers and no body.
    let mut status;
    let mut headers;
    let mut interim = 0;
    loop {
        status = read_status_line(reader)?;
        headers = read_headers(reader)?;
        if (100..200).contains(&status) {
            interim += 1;
            if interim > 5 {
                return Err(TransportError::Protocol);
            }
            continue;
        }
        break;
    }

    let no_body = method == "HEAD" || status == 204 || status == 304;
    if no_body {
        return Ok(WireResponse {
            status,
            headers,
            body: Vec::new(),
        });
    }

    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .to_ascii_lowercase()
                .split(',')
                .any(|token| token.trim() == "chunked")
    });
    let body = if chunked {
        read_chunked(reader, max_response_bytes)?
    } else if let Some(length) = content_length(&headers)? {
        if length > max_response_bytes as u64 {
            return Err(TransportError::TooLarge);
        }
        let mut body = vec![0u8; length as usize];
        reader.read_exact(&mut body).map_err(io_transport)?;
        body
    } else {
        read_to_eof(reader, max_response_bytes)?
    };
    Ok(WireResponse {
        status,
        headers,
        body,
    })
}

fn read_status_line<R: Read>(reader: &mut BufReader<R>) -> Result<u16, TransportError> {
    let line = read_line(reader, 8192)?;
    let text = std::str::from_utf8(&line).map_err(|_| TransportError::Protocol)?;
    let mut parts = text.trim_end_matches(['\r', '\n']).split_whitespace();
    let version = parts.next().ok_or(TransportError::Protocol)?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(TransportError::Protocol);
    }
    let status: u16 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .filter(|value| (100..=599).contains(value))
        .ok_or(TransportError::Protocol)?;
    Ok(status)
}

fn read_headers<R: Read>(
    reader: &mut BufReader<R>,
) -> Result<Vec<(String, String)>, TransportError> {
    let mut headers = Vec::new();
    let mut total = 0usize;
    loop {
        let line = read_line(reader, 8192)?;
        total += line.len();
        if total > 65_536 {
            return Err(TransportError::Protocol);
        }
        let text = std::str::from_utf8(&line).map_err(|_| TransportError::Protocol)?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            return Ok(headers);
        }
        let (name, value) = text.split_once(':').ok_or(TransportError::Protocol)?;
        if !valid_header_name(name) {
            return Err(TransportError::Protocol);
        }
        if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err(TransportError::Protocol);
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
}

fn content_length(headers: &[(String, String)]) -> Result<Option<u64>, TransportError> {
    let mut length = None;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            let parsed: u64 = value.parse().map_err(|_| TransportError::Protocol)?;
            if length.is_some_and(|existing| existing != parsed) {
                return Err(TransportError::Protocol);
            }
            length = Some(parsed);
        }
    }
    Ok(length)
}

fn read_chunked<R: Read>(
    reader: &mut BufReader<R>,
    max_response_bytes: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, 1024)?;
        let text = std::str::from_utf8(&line).map_err(|_| TransportError::Protocol)?;
        let size_text = text
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| TransportError::Protocol)?;
        if size == 0 {
            // A bounded, ignored trailer section.
            let mut trailer = 0usize;
            loop {
                let line = read_line(reader, 8192)?;
                if trailer.saturating_add(line.len()) > 8192 {
                    return Err(TransportError::Protocol);
                }
                trailer += line.len();
                let text = std::str::from_utf8(&line).map_err(|_| TransportError::Protocol)?;
                if text.trim_end_matches(['\r', '\n']).is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len().saturating_add(size) > max_response_bytes {
            return Err(TransportError::TooLarge);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(io_transport)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).map_err(io_transport)?;
        if crlf != *b"\r\n" {
            return Err(TransportError::Protocol);
        }
    }
}

fn read_to_eof<R: Read>(
    reader: &mut BufReader<R>,
    max_response_bytes: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut body = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).map_err(io_transport)?;
        if read == 0 {
            return Ok(body);
        }
        if body.len().saturating_add(read) > max_response_bytes {
            return Err(TransportError::TooLarge);
        }
        body.extend_from_slice(&buffer[..read]);
    }
}

/// Reads one CRLF-terminated line, never growing past `max`. `read_until` grows
/// the buffer until the delimiter arrives, so a peer that never sends one can
/// grow it without bound even though the line is refused afterwards; this
/// checks the bound before each copy and consumes at most one buffered chunk
/// beyond it.
fn read_line<R: Read>(reader: &mut BufReader<R>, max: usize) -> Result<Vec<u8>, TransportError> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().map_err(io_transport)?;
        if buffer.is_empty() {
            return Err(TransportError::Protocol);
        }
        let (take, done) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (buffer.len(), false),
        };
        if line.len().saturating_add(take) > max {
            return Err(TransportError::Protocol);
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if done {
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    const PUBLIC: &str = "93.184.216.34";

    fn public() -> IpAddr {
        PUBLIC.parse().unwrap()
    }

    /// A resolver that returns a fixed sequence of answers and counts calls. A
    /// second call for the same request consumes the next answer, which is how
    /// the pinning tests detect a hidden re-resolution.
    struct SequenceResolver {
        answers: Mutex<VecDeque<Vec<IpAddr>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl SequenceResolver {
        fn new(answers: Vec<Vec<IpAddr>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn pending(&self) -> usize {
            self.answers.lock().unwrap().len()
        }
    }

    impl Resolver for SequenceResolver {
        fn resolve<'a>(&'a self, _host: &'a str, _port: u16) -> ResolveFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(self.answers.lock().unwrap().pop_front().unwrap_or_default())
            })
        }
    }

    /// An injected transport: it records the exact `Connection` it was handed
    /// and returns canned responses in order (or one fixed failure).
    struct FakeTransport {
        responses: Mutex<VecDeque<WireResponse>>,
        failure: Option<TransportError>,
        calls: Mutex<Vec<Connection>>,
        delay: Duration,
    }

    impl FakeTransport {
        fn new(responses: Vec<WireResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                failure: None,
                calls: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            }
        }

        fn failing(failure: TransportError) -> Self {
            Self {
                failure: Some(failure),
                ..Self::new(Vec::new())
            }
        }

        fn slow(delay: Duration) -> Self {
            Self {
                delay,
                ..Self::new(Vec::new())
            }
        }

        fn calls(&self) -> Vec<Connection> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl HttpTransport for FakeTransport {
        fn round_trip<'a>(
            &'a self,
            connection: &'a Connection,
            _request: &'a WireRequest,
            _deadline: Instant,
            _max_response_bytes: usize,
        ) -> TransportFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(connection.clone());
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                if let Some(failure) = self.failure {
                    return Err(failure);
                }
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or(TransportError::Protocol)
            })
        }
    }

    fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> WireResponse {
        WireResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: body.to_vec(),
        }
    }

    fn ok(body: &[u8]) -> WireResponse {
        response(200, &[("content-type", "application/json")], body)
    }

    fn redirect(location: &str) -> WireResponse {
        response(302, &[("location", location)], b"")
    }

    fn make_client(
        allowlist: &[&str],
        allow_private: bool,
        allow_metadata: bool,
        transport: Arc<FakeTransport>,
        resolver: Arc<SequenceResolver>,
    ) -> OutboundClient {
        OutboundClient::new(
            EgressPolicy::new(
                &allowlist
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
                allow_private,
                allow_metadata,
            )
            .unwrap(),
            Limits {
                timeout: Duration::from_secs(2),
                max_response_bytes: 64 * 1024,
                max_request_bytes: 64 * 1024,
                max_redirects: 5,
            },
            transport,
            resolver,
        )
    }

    #[test]
    fn every_required_range_and_metadata_endpoint_is_classified() {
        for address in [
            "127.0.0.1",
            "127.255.255.254",
            "::1",
            "169.254.10.20",
            "fe80::1",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.4.5",
            "100.64.0.1",
            "fc00::1",
            "fd12:3456::1",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
            "198.18.0.1",
            "0.0.0.1",
            "::127.0.0.1",
            "::ffff:0:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "2002:a9fe:a9fe::",
            "2002:7f00:1::",
        ] {
            let address: IpAddr = address.parse().unwrap();
            assert_ne!(
                classify_address(address),
                AddressClass::Global,
                "{address} must not be global"
            );
        }
        for (address, class) in [
            ("127.0.0.1", AddressClass::Loopback),
            ("::1", AddressClass::Loopback),
            ("169.254.10.20", AddressClass::LinkLocal),
            ("fe80::1", AddressClass::LinkLocal),
            ("10.0.0.5", AddressClass::Private),
            ("172.20.1.1", AddressClass::Private),
            ("192.168.1.1", AddressClass::Private),
            ("100.100.100.200", AddressClass::Metadata),
            ("169.254.169.254", AddressClass::Metadata),
            ("169.254.170.2", AddressClass::Metadata),
            ("168.63.129.16", AddressClass::Metadata),
            ("fd00:ec2::254", AddressClass::Metadata),
            ("::ffff:127.0.0.1", AddressClass::Loopback),
            ("::127.0.0.1", AddressClass::Loopback),
            ("::ffff:0:127.0.0.1", AddressClass::Loopback),
            ("::ffff:169.254.169.254", AddressClass::Metadata),
            ("64:ff9b::a9fe:a9fe", AddressClass::Metadata),
            ("2002:a9fe:a9fe::", AddressClass::Metadata),
            ("2002:7f00:1::", AddressClass::Loopback),
            ("64:ff9b:1::1", AddressClass::Reserved),
            ("198.18.0.1", AddressClass::Reserved),
        ] {
            assert_eq!(
                classify_address(address.parse().unwrap()),
                class,
                "{address}"
            );
        }
        assert_eq!(
            classify_address("93.184.216.34".parse().unwrap()),
            AddressClass::Global
        );
        assert_eq!(
            classify_address("2001:4860:4860::8888".parse().unwrap()),
            AddressClass::Global
        );
    }

    #[test]
    fn the_allowlist_is_exact_or_a_subdomain_wildcard_with_an_optional_port() {
        let policy = EgressPolicy::new(
            &[
                "api.example.com".into(),
                "*.hooks.example.com".into(),
                "secure.example.net:8443".into(),
            ],
            false,
            false,
        )
        .unwrap();
        assert!(policy.permits("api.example.com", 443).is_some());
        assert!(
            policy.permits("api.example.com", 80).is_some(),
            "a host-only entry covers the standard ports"
        );
        assert!(policy.permits("API.EXAMPLE.COM", 443).is_some());
        assert!(
            policy.permits("api.example.com", 22).is_none(),
            "a host-only entry must not open every port"
        );
        assert!(policy.permits("api.example.com", 8080).is_none());
        assert!(policy.permits("a.hooks.example.com", 443).is_some());
        assert!(
            policy.permits("hooks.example.com", 443).is_none(),
            "a wildcard does not cover its apex"
        );
        assert!(policy.permits("a.b.hooks.example.com", 443).is_some());
        assert!(policy.permits("secure.example.net", 8443).is_some());
        assert!(policy.permits("secure.example.net", 443).is_none());
        for refused in [
            "example.com",
            "evil.evil.example.com",
            "api.example.com.evil.test",
            "notapi.example.com",
        ] {
            assert!(policy.permits(refused, 443).is_none(), "{refused}");
        }
    }

    #[test]
    fn an_allowlist_entry_is_a_dns_name_with_a_registrable_wildcard() {
        for entry in ["*.example.com", "api.example.com", "api.example.com:8443"] {
            assert!(validate_allowlist_entry(entry).is_ok(), "{entry}");
        }
        for entry in [
            "https://api.example.com",
            "api.example.com/path",
            "user@api.example.com",
            "*.*.example.com",
            "api example.com",
            "api.example.com:0",
            "",
            "*.com",
            "10.0.0.5",
            "169.254.169.254",
            "127.1",
            "2130706433",
            "0x7f.1",
        ] {
            assert!(
                validate_allowlist_entry(entry).is_err(),
                "{entry} must not parse"
            );
        }
        // Two labels under the wildcard is the minimum; `*.co.uk` is still
        // broad, and narrowing a public suffix is left to the operator.
        assert!(validate_allowlist_entry("*.co.uk").is_ok());
    }

    #[test]
    fn metadata_needs_its_own_option_and_private_does_not_open_it() {
        let private = EgressPolicy::new(&[], true, false).unwrap();
        assert!(private.permits_address("10.0.0.5".parse().unwrap()).is_ok());
        assert!(
            private
                .permits_address("127.0.0.1".parse().unwrap())
                .is_ok()
        );
        let error = private
            .permits_address("169.254.169.254".parse().unwrap())
            .unwrap_err();
        assert!(error.contains("outbound_allow_metadata"), "{error}");

        let metadata = EgressPolicy::new(&[], false, true).unwrap();
        assert!(
            metadata
                .permits_address("169.254.169.254".parse().unwrap())
                .is_ok()
        );
        assert!(
            metadata
                .permits_address("10.0.0.5".parse().unwrap())
                .is_err(),
            "metadata alone does not open private ranges"
        );
    }

    #[test]
    fn a_destination_must_be_a_named_http_or_https_origin() {
        let parsed = Destination::parse("HTTPS://API.Example.COM./v1/keys?x=1#frag").unwrap();
        assert_eq!(parsed.scheme, Scheme::Https);
        assert_eq!(parsed.host, "api.example.com");
        assert_eq!(parsed.port, 443);
        assert_eq!(parsed.path, "/v1/keys?x=1");

        for url in [
            "ftp://api.example.com/",
            "https://user:pass@api.example.com/",
            "https://169.254.169.254/latest/meta-data/",
            "https://[::1]/",
            "https://api.example.com:0/",
            "https://bad host/",
            "https:///path",
            "https://api.example.com/a b",
        ] {
            assert!(Destination::parse(url).is_err(), "{url} must be refused");
        }
    }

    /// `getaddrinfo` accepts these spellings even though `IpAddr` does not, so
    /// refusing the spelling is what keeps every destination a name.
    #[test]
    fn a_legacy_numeric_host_is_refused_as_an_ip_literal() {
        for host in [
            "127.1",
            "2130706433",
            "0x7f.1",
            "0300.0250.1.1",
            "127.0.0.1",
        ] {
            let error = Destination::parse(&format!("https://{host}/discovery"))
                .expect_err("{host} must be refused");
            assert!(
                matches!(error, OutboundError::InvalidRequest(ref reason) if reason.contains("IP-literal")),
                "{host}: {error:?}"
            );
        }
        // A name that merely begins with digits is still a name.
        assert!(Destination::parse("https://1.example.test/").is_ok());
    }

    #[test]
    fn a_same_host_redirect_can_be_relative_and_keeps_the_origin() {
        let current = Destination::parse("https://api.example.com/v1/discovery").unwrap();
        assert_eq!(
            resolve_location(&current, "/v2/discovery")
                .unwrap()
                .to_string(),
            "https://api.example.com/v2/discovery"
        );
        assert_eq!(
            resolve_location(&current, "next").unwrap().to_string(),
            "https://api.example.com/v1/next"
        );
        assert_eq!(
            resolve_location(&current, "//api.example.com/other")
                .unwrap()
                .to_string(),
            "https://api.example.com/other"
        );
        assert_eq!(
            resolve_location(&current, "https://api.example.com/abs")
                .unwrap()
                .to_string(),
            "https://api.example.com/abs"
        );
        assert_eq!(
            resolve_location(&current, "HTTPS://other.example/abs")
                .unwrap()
                .to_string(),
            "https://other.example/abs",
            "an uppercase scheme is absolute, not a relative path"
        );
    }

    #[test]
    fn caller_headers_cannot_rewrite_framing_or_the_destination() {
        let headers = sanitize_headers(&[
            ("authorization".into(), "Bearer secret".into()),
            ("Host".into(), "evil.example".into()),
            ("Content-Length".into(), "999".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("x-request-id".into(), "abc".into()),
        ])
        .unwrap();
        assert_eq!(
            headers,
            vec![
                ("authorization".into(), "Bearer secret".into()),
                ("x-request-id".into(), "abc".into())
            ]
        );
        assert!(
            sanitize_headers(&[("x".into(), "a\r\nHost: evil".into())]).is_err(),
            "a header line break is refused"
        );
    }

    #[test]
    fn the_response_reader_bounds_a_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n0\r\n\r\n";
        let mut reader = BufReader::new(std::io::Cursor::new(raw.to_vec()));
        let response = read_response(&mut reader, "GET", 1024).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");

        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789";
        let mut reader = BufReader::new(std::io::Cursor::new(raw.to_vec()));
        assert_eq!(
            read_response(&mut reader, "GET", 9),
            Err(TransportError::TooLarge)
        );
    }

    /// A reader that produces `remaining` bytes and never a newline. It counts
    /// what the line reader actually consumed, which is the allocation bound.
    struct EndlessLine {
        remaining: usize,
        consumed: usize,
    }

    impl Read for EndlessLine {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let take = buffer.len().min(self.remaining);
            buffer[..take].fill(b'x');
            self.remaining -= take;
            self.consumed += take;
            Ok(take)
        }
    }

    #[test]
    fn a_single_response_line_is_bounded_before_it_is_allocated() {
        const LINE: usize = 4 * 1024 * 1024;
        let mut reader = BufReader::new(EndlessLine {
            remaining: LINE,
            consumed: 0,
        });
        let error = read_line(&mut reader, 64).unwrap_err();
        assert_eq!(error, TransportError::Protocol);
        // `read_until` would consume all 4 MiB before refusing; the bounded
        // reader stops at the first buffered chunk.
        assert!(
            reader.get_ref().consumed <= 8192,
            "the line reader consumed {} bytes for a 64 byte bound",
            reader.get_ref().consumed
        );
    }

    #[test]
    fn the_transport_offers_no_alpn_so_there_is_no_http2_reuse() {
        let config = tls_config();
        assert!(
            config.alpn_protocols.is_empty(),
            "ALPN must be empty: HTTP/2 would let one connection carry several requests"
        );
    }

    #[test]
    fn an_smtp_destination_uses_the_shared_address_policy() {
        let policy = EgressPolicy::new(&[], false, false).unwrap();
        let metadata = choose_operator_address(
            "smtp.example.test",
            vec!["169.254.169.254:25".parse().unwrap()],
            &policy,
        )
        .unwrap_err();
        assert!(metadata.contains("metadata"), "{metadata}");

        let loopback = choose_operator_address(
            "smtp.example.test",
            vec!["127.0.0.1:25".parse().unwrap()],
            &policy,
        )
        .unwrap_err();
        assert!(loopback.contains("loopback"), "{loopback}");

        let private = EgressPolicy::new(&[], true, false).unwrap();
        assert_eq!(
            choose_operator_address(
                "smtp.internal",
                vec!["10.0.0.5:25".parse().unwrap()],
                &private
            )
            .unwrap(),
            "10.0.0.5:25".parse().unwrap()
        );

        let empty = choose_operator_address("smtp.example.test", Vec::new(), &policy).unwrap_err();
        assert!(empty.contains("no usable address"), "{empty}");
    }

    #[tokio::test]
    async fn every_refused_range_and_the_metadata_endpoint_is_refused_and_recorded() {
        let cases = [
            ("127.0.0.1", AddressClass::Loopback, "loopback"),
            ("::1", AddressClass::Loopback, "loopback"),
            ("169.254.10.20", AddressClass::LinkLocal, "link-local"),
            ("fe80::1", AddressClass::LinkLocal, "link-local"),
            ("10.1.2.3", AddressClass::Private, "private-range"),
            ("172.20.0.1", AddressClass::Private, "private-range"),
            ("192.168.4.5", AddressClass::Private, "private-range"),
            ("fc00::1", AddressClass::Private, "private-range"),
            ("0.0.0.0", AddressClass::Unspecified, "unspecified"),
            ("224.0.0.1", AddressClass::Multicast, "multicast"),
            ("169.254.169.254", AddressClass::Metadata, "metadata"),
            ("169.254.170.2", AddressClass::Metadata, "metadata"),
            ("100.100.100.200", AddressClass::Metadata, "metadata"),
            ("168.63.129.16", AddressClass::Metadata, "metadata"),
            ("192.0.0.192", AddressClass::Metadata, "metadata"),
            ("fd00:ec2::254", AddressClass::Metadata, "metadata"),
            ("::ffff:169.254.169.254", AddressClass::Metadata, "metadata"),
            ("64:ff9b::a9fe:a9fe", AddressClass::Metadata, "metadata"),
            ("2002:a9fe:a9fe::", AddressClass::Metadata, "metadata"),
            ("::127.0.0.1", AddressClass::Loopback, "loopback"),
            ("::ffff:0:127.0.0.1", AddressClass::Loopback, "loopback"),
            ("2002:7f00:1::", AddressClass::Loopback, "loopback"),
            ("198.18.0.1", AddressClass::Reserved, "reserved"),
        ];
        for (answer, expected_class, reason) in cases {
            let answer: IpAddr = answer.parse().unwrap();
            let transport = Arc::new(FakeTransport::new(vec![ok(b"unreachable")]));
            let resolver = Arc::new(SequenceResolver::new(vec![vec![answer]]));
            let client = make_client(
                &["api.example.test"],
                false,
                false,
                transport.clone(),
                resolver,
            );
            let error = client
                .send(OutboundRequest::get(
                    "https://api.example.test/.well-known/openid-configuration",
                ))
                .await
                .expect_err("a non-global answer must be refused");
            match error {
                OutboundError::AddressRefused { address, class, .. } => {
                    assert_eq!(address, answer);
                    assert_eq!(class, expected_class, "{answer}");
                }
                other => panic!("{answer}: expected AddressRefused, got {other:?}"),
            }
            assert!(
                transport.calls().is_empty(),
                "a refused answer must never reach the transport ({answer})"
            );
            let records = client.records();
            let last = records.last().expect("a refusal is recorded");
            assert!(!last.allowed);
            assert_eq!(last.address, Some(answer));
            assert!(
                last.reason
                    .to_ascii_lowercase()
                    .contains(&reason.to_ascii_lowercase()),
                "{answer}: {last:?}"
            );
        }
    }

    /// The three names every cloud documents as its metadata service. They are
    /// refused because the answer is checked, so the name is recorded and no
    /// socket is opened.
    #[tokio::test]
    async fn metadata_hosts_are_refused_by_name_and_by_address() {
        for (host, address) in [
            ("metadata.google.internal", "169.254.169.254"),
            ("instance-data.ec2.internal", "169.254.169.254"),
            ("metadata.azure.com", "169.254.169.254"),
        ] {
            let answer: IpAddr = address.parse().unwrap();
            let transport = Arc::new(FakeTransport::new(vec![ok(b"credentials")]));
            let resolver = Arc::new(SequenceResolver::new(vec![vec![answer]]));
            let client = make_client(&[host], true, false, transport.clone(), resolver.clone());
            let error = client
                .send(OutboundRequest::get(format!(
                    "https://{host}/latest/meta-data/"
                )))
                .await
                .expect_err("a metadata name must be refused");
            assert!(matches!(
                error,
                OutboundError::AddressRefused {
                    class: AddressClass::Metadata,
                    ..
                }
            ));
            assert_eq!(
                resolver.calls(),
                1,
                "the name is resolved once, then checked"
            );
            assert!(transport.calls().is_empty());
            let record = client.records().last().cloned().unwrap();
            assert!(!record.allowed);
            assert_eq!(record.host, host);
            assert_eq!(record.address, Some(answer));
            assert!(record.reason.contains("metadata"), "{record:?}");
            assert!(record.reason.contains("outbound_allow_metadata"));
        }
    }

    #[tokio::test]
    async fn one_forbidden_answer_refuses_the_whole_resolution() {
        let transport = Arc::new(FakeTransport::new(vec![ok(b"unreachable")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![
            public(),
            "127.0.0.1".parse().unwrap(),
        ]]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver,
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/"))
            .await
            .expect_err("a mixed answer is refused as a whole");
        assert!(matches!(
            error,
            OutboundError::AddressRefused {
                class: AddressClass::Loopback,
                ..
            }
        ));
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn an_allowlisted_destination_succeeds_and_the_allow_is_recorded() {
        let transport = Arc::new(FakeTransport::new(vec![ok(
            br#"{"issuer":"https://api.example.test"}"#,
        )]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let response = client
            .send(OutboundRequest::get(
                "https://api.example.test/.well-known/openid-configuration",
            ))
            .await
            .expect("an allowlisted destination succeeds");
        assert_eq!(response.status, 200);
        assert_eq!(response.hops, 1);
        assert_eq!(
            response.final_url,
            "https://api.example.test/.well-known/openid-configuration"
        );

        let calls = transport.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].address,
            SocketAddr::new(public(), 443),
            "the transport receives the resolved, checked address"
        );
        assert_eq!(calls[0].server_name, "api.example.test");
        assert!(calls[0].tls);
        assert_eq!(resolver.calls(), 1, "one resolution per hop");

        let record = client
            .records()
            .last()
            .cloned()
            .expect("the allow is recorded");
        assert!(record.allowed);
        assert_eq!(record.host, "api.example.test");
        assert_eq!(record.address, Some(public()));
        assert!(record.reason.contains("allowlisted"), "{record:?}");
        assert!(record.reason.contains("pinned"), "{record:?}");
    }

    #[tokio::test]
    async fn a_dns_answer_that_changes_after_the_check_cannot_redirect_the_connection() {
        // First answer is public and is the one checked. The second would be
        // loopback: if the client re-resolved after the check (or the transport
        // did) it would either connect to loopback or be refused. Neither
        // happens.
        let transport = Arc::new(FakeTransport::new(vec![ok(b"first"), ok(b"second")]));
        let resolver = Arc::new(SequenceResolver::new(vec![
            vec![public()],
            vec!["127.0.0.1".parse().unwrap()],
        ]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );

        let first = client
            .send(OutboundRequest::get("https://api.example.test/discovery"))
            .await
            .expect("the checked answer is allowed");
        assert_eq!(first.body, b"first");
        assert_eq!(resolver.calls(), 1, "the name is resolved once per hop");
        let calls = transport.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].address,
            SocketAddr::new(public(), 443),
            "the connection goes to the address that was checked"
        );
        let allow = client.records().last().cloned().unwrap();
        assert!(allow.allowed);
        assert_eq!(allow.address, Some(public()));
        assert!(
            allow.reason.contains("pinned"),
            "the record names the pin: {allow:?}"
        );

        // On the next request the changed answer is checked afresh and refused,
        // so the pin protects the in-flight connection rather than caching an
        // answer forever. The transport is not reached a second time.
        let error = client
            .send(OutboundRequest::get("https://api.example.test/discovery"))
            .await
            .expect_err("the new loopback answer must be refused");
        assert!(matches!(
            error,
            OutboundError::AddressRefused {
                class: AddressClass::Loopback,
                ..
            }
        ));
        assert_eq!(transport.calls().len(), 1);
        assert_eq!(resolver.calls(), 2);
    }

    #[tokio::test]
    async fn a_dns_answer_that_changes_on_a_redirect_hop_is_refused() {
        let transport = Arc::new(FakeTransport::new(vec![
            redirect("https://api.example.test/next"),
            ok(b"should not be reached"),
        ]));
        let resolver = Arc::new(SequenceResolver::new(vec![
            vec![public()],
            vec!["127.0.0.1".parse().unwrap()],
        ]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/first"))
            .await
            .expect_err("the redirect hop's changed answer is refused");
        assert!(matches!(
            error,
            OutboundError::AddressRefused {
                class: AddressClass::Loopback,
                ..
            }
        ));
        assert_eq!(resolver.calls(), 2, "each hop resolves afresh");
        assert_eq!(
            transport.calls().len(),
            1,
            "the hop whose answer changed is never contacted"
        );
        assert!(
            client
                .records()
                .last()
                .is_some_and(|record| !record.allowed && record.reason.contains("loopback"))
        );
    }

    #[tokio::test]
    async fn a_redirect_to_a_different_host_is_refused_and_recorded() {
        let transport = Arc::new(FakeTransport::new(vec![redirect(
            "https://evil.example/steal",
        )]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["api.example.test", "evil.example"],
            false,
            false,
            transport.clone(),
            resolver,
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/hook"))
            .await
            .expect_err("a cross-host redirect must be refused");
        match error {
            OutboundError::RedirectToDifferentHost { from, to } => {
                assert_eq!(from, "api.example.test");
                assert_eq!(to, "evil.example");
            }
            other => panic!("expected RedirectToDifferentHost, got {other:?}"),
        }
        assert_eq!(
            transport.calls().len(),
            1,
            "the different host is never contacted"
        );
        let record = client.records().last().cloned().unwrap();
        assert!(!record.allowed);
        assert!(record.reason.contains("different host"), "{record:?}");
    }

    #[tokio::test]
    async fn an_uppercase_scheme_redirect_is_absolute_and_cross_host_is_refused() {
        let transport = Arc::new(FakeTransport::new(vec![redirect(
            "HTTPS://evil.example/steal",
        )]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["api.example.test", "evil.example"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/hook"))
            .await
            .expect_err("a cross-host redirect written in uppercase is still cross-host");
        assert!(matches!(
            error,
            OutboundError::RedirectToDifferentHost { .. }
        ));
        assert_eq!(resolver.calls(), 1);
        assert_eq!(transport.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_same_host_redirect_is_followed_but_cannot_change_the_scheme() {
        let transport = Arc::new(FakeTransport::new(vec![
            redirect("/v2/discovery"),
            ok(b"second"),
        ]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()], vec![public()]]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let response = client
            .send(OutboundRequest::get(
                "https://api.example.test/v1/discovery",
            ))
            .await
            .expect("a same-host redirect is followed");
        assert_eq!(response.body, b"second");
        assert_eq!(response.hops, 2);
        assert_eq!(response.final_url, "https://api.example.test/v2/discovery");
        assert_eq!(resolver.calls(), 2, "the redirect hop is resolved afresh");

        // A redirect from https to http is a scheme change and is refused.
        let transport = Arc::new(FakeTransport::new(vec![redirect(
            "http://api.example.test/v1/discovery",
        )]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver,
        );
        let error = client
            .send(OutboundRequest::get(
                "https://api.example.test/v1/discovery",
            ))
            .await
            .expect_err("a scheme change must be refused");
        assert!(matches!(error, OutboundError::RedirectSchemeChanged { .. }));
        assert_eq!(transport.calls().len(), 1);
        assert!(
            client
                .records()
                .last()
                .is_some_and(|record| !record.allowed && record.reason.contains("scheme"))
        );
    }

    #[tokio::test]
    async fn an_unlisted_destination_is_refused_before_resolution() {
        let transport = Arc::new(FakeTransport::new(vec![ok(b"unreachable")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["other.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/discovery"))
            .await
            .expect_err("an unlisted destination is refused");
        assert!(matches!(error, OutboundError::NotAllowlisted { .. }));
        assert_eq!(resolver.calls(), 0, "an unlisted name is not even resolved");
        assert!(transport.calls().is_empty());
        assert!(
            client
                .records()
                .last()
                .is_some_and(|record| !record.allowed && record.reason.contains("allowlist"))
        );
    }

    #[tokio::test]
    async fn a_legacy_numeric_destination_is_refused_before_resolution() {
        for host in ["127.1", "2130706433", "0x7f.1"] {
            let transport = Arc::new(FakeTransport::new(vec![ok(b"unreachable")]));
            let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
            let client = make_client(
                &["numeric.example.test"],
                true,
                true,
                transport.clone(),
                resolver.clone(),
            );
            let error = client
                .send(OutboundRequest::get(format!("https://{host}/")))
                .await
                .expect_err("{host} must be refused as an IP literal");
            assert!(matches!(error, OutboundError::InvalidRequest(_)));
            assert_eq!(resolver.calls(), 0);
            assert!(transport.calls().is_empty());
        }
    }

    #[tokio::test]
    async fn private_ranges_need_the_operator_option_and_metadata_keeps_its_own() {
        // With the private option, a private answer succeeds and the connection
        // is pinned to it.
        let transport = Arc::new(FakeTransport::new(vec![ok(b"internal")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![
            "10.0.0.5".parse().unwrap(),
        ]]));
        let client = make_client(&["idp.internal"], true, false, transport.clone(), resolver);
        client
            .send(OutboundRequest::get("https://idp.internal/discovery"))
            .await
            .expect("a private destination is allowed when the operator says so");
        assert_eq!(
            transport.calls()[0].address,
            SocketAddr::new("10.0.0.5".parse().unwrap(), 443)
        );
        assert!(client.records().last().is_some_and(
            |record| record.allowed && record.reason.contains("outbound_allow_private")
        ));

        // The private option does not open a metadata endpoint.
        let transport = Arc::new(FakeTransport::new(vec![ok(b"secret")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![
            "169.254.169.254".parse().unwrap(),
        ]]));
        let client = make_client(&["idp.internal"], true, false, transport.clone(), resolver);
        let error = client
            .send(OutboundRequest::get("https://idp.internal/"))
            .await
            .expect_err("metadata is not private-range-only");
        assert!(matches!(
            error,
            OutboundError::AddressRefused {
                class: AddressClass::Metadata,
                ..
            }
        ));
        assert!(transport.calls().is_empty());

        // The metadata option opens it explicitly.
        let transport = Arc::new(FakeTransport::new(vec![ok(b"metadata")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![
            "169.254.169.254".parse().unwrap(),
        ]]));
        let client = make_client(&["idp.internal"], false, true, transport.clone(), resolver);
        client
            .send(OutboundRequest::get("https://idp.internal/"))
            .await
            .expect("outbound_allow_metadata permits the endpoint");
        assert_eq!(transport.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_response_over_the_bound_is_refused_cleanly() {
        let transport = Arc::new(FakeTransport::new(vec![ok(b"0123456789")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = OutboundClient::new(
            EgressPolicy::new(&["api.example.test".into()], false, false).unwrap(),
            Limits {
                timeout: Duration::from_secs(2),
                max_response_bytes: 9,
                max_request_bytes: 64 * 1024,
                max_redirects: 5,
            },
            transport,
            resolver,
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/"))
            .await
            .expect_err("an oversized response is refused");
        assert_eq!(error, OutboundError::ResponseTooLarge { limit: 9 });
        assert!(
            client
                .records()
                .last()
                .is_some_and(|record| !record.allowed && record.reason.contains("9 byte"))
        );

        // A request body over the bound is refused before any resolution.
        let transport = Arc::new(FakeTransport::new(vec![ok(b"unreachable")]));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = OutboundClient::new(
            EgressPolicy::new(&["api.example.test".into()], false, false).unwrap(),
            Limits {
                timeout: Duration::from_secs(2),
                max_response_bytes: 1024,
                max_request_bytes: 4,
                max_redirects: 5,
            },
            transport.clone(),
            resolver.clone(),
        );
        let error = client
            .send(OutboundRequest::post(
                "https://api.example.test/hook",
                "application/json",
                b"12345".to_vec(),
            ))
            .await
            .expect_err("an oversized request is refused");
        assert_eq!(error, OutboundError::RequestTooLarge { limit: 4 });
        assert_eq!(resolver.calls(), 0);
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn a_timeout_is_refused_cleanly_and_recorded() {
        let transport = Arc::new(FakeTransport::slow(Duration::from_secs(5)));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = OutboundClient::new(
            EgressPolicy::new(&["api.example.test".into()], false, false).unwrap(),
            Limits {
                timeout: Duration::from_millis(50),
                max_response_bytes: 1024,
                max_request_bytes: 1024,
                max_redirects: 5,
            },
            transport,
            resolver,
        );
        let started = Instant::now();
        let error = client
            .send(OutboundRequest::get("https://api.example.test/"))
            .await
            .expect_err("a slow request is refused at the bound");
        assert!(matches!(error, OutboundError::Timeout { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bound is the client's, not the transport's"
        );
        assert!(
            client
                .records()
                .last()
                .is_some_and(|record| !record.allowed && record.reason.contains("timed out"))
        );
    }

    #[tokio::test]
    async fn a_transport_deadline_is_a_timeout_and_a_failure_is_not_retried() {
        let transport = Arc::new(FakeTransport::failing(TransportError::Timeout));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/"))
            .await
            .expect_err("a transport deadline is a timeout");
        assert!(matches!(error, OutboundError::Timeout { .. }));
        assert!(
            client
                .records()
                .last()
                .is_some_and(|record| !record.allowed && record.reason.contains("timed out"))
        );
        assert_eq!(resolver.calls(), 1);

        let transport = Arc::new(FakeTransport::failing(TransportError::Connect));
        let resolver = Arc::new(SequenceResolver::new(vec![vec![public()]]));
        let client = make_client(
            &["api.example.test"],
            false,
            false,
            transport.clone(),
            resolver.clone(),
        );
        let error = client
            .send(OutboundRequest::get("https://api.example.test/"))
            .await
            .expect_err("a connect failure is refused");
        assert_eq!(error, OutboundError::Transport);
        assert_eq!(transport.calls().len(), 1, "a failure is not retried");
        assert_eq!(resolver.calls(), 1, "a failure does not re-resolve");
    }

    /// One request reads the request head, answers 200, and reports the request
    /// text and the peer address. The listener is closed when the thread ends.
    fn one_request_server(listener: TcpListener) -> thread::JoinHandle<(SocketAddr, String)> {
        thread::spawn(move || {
            let (mut stream, peer) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            (peer, String::from_utf8_lossy(&request).into_owned())
        })
    }

    /// The pin, proved over a real socket: a name resolves to the server that
    /// answers, the name's *next* answer is a different address, and the request
    /// still arrives at the checked address with exactly one resolution.
    #[tokio::test]
    async fn a_resolved_answer_is_pinned_to_the_connected_address_over_a_real_socket() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = one_request_server(listener);
        let resolver = Arc::new(SequenceResolver::new(vec![
            vec!["127.0.0.1".parse().unwrap()],
            // The changed answer: TEST-NET-1, where nothing is listening. Any
            // re-resolution or retry would try this and fail instead.
            vec!["192.0.2.1".parse().unwrap()],
        ]));
        let client = OutboundClient::new(
            EgressPolicy::new(&[format!("pinned.example.test:{port}")], true, false).unwrap(),
            Limits {
                timeout: Duration::from_secs(2),
                max_response_bytes: 1024,
                max_request_bytes: 1024,
                max_redirects: 2,
            },
            Arc::new(TcpTransport),
            resolver.clone(),
        );
        let response = client
            .send(OutboundRequest::get(format!(
                "http://pinned.example.test:{port}/discovery"
            )))
            .await
            .expect("the checked address answers");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");

        let (peer, request) = server.join().unwrap();
        assert_eq!(
            peer.ip(),
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            "the connection went to the address that was checked"
        );
        assert!(
            request.starts_with("GET /discovery HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(&format!("Host: pinned.example.test:{port}\r\n")),
            "{request}"
        );
        assert!(
            request.contains("Connection: close\r\n"),
            "one request per connection: {request}"
        );
        assert_eq!(resolver.calls(), 1, "the transport did not re-resolve");
        assert_eq!(
            resolver.pending(),
            1,
            "the changed answer was never consumed"
        );

        let record = client.records().last().cloned().unwrap();
        assert!(record.allowed);
        assert_eq!(record.address, Some("127.0.0.1".parse().unwrap()));
        assert!(record.reason.contains("pinned"), "{record:?}");
    }

    /// A redirect is a new request and a new connection; the client never
    /// reuses the first hop's socket.
    #[tokio::test]
    async fn a_same_host_redirect_opens_a_new_connection() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                requests.push(String::from_utf8_lossy(&request).into_owned());
                if index == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /second\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
            requests
        });
        let resolver = Arc::new(SequenceResolver::new(vec![
            vec!["127.0.0.1".parse().unwrap()],
            vec!["127.0.0.1".parse().unwrap()],
        ]));
        let client = OutboundClient::new(
            EgressPolicy::new(&[format!("pinned.example.test:{port}")], true, false).unwrap(),
            Limits {
                timeout: Duration::from_secs(2),
                max_response_bytes: 1024,
                max_request_bytes: 1024,
                max_redirects: 2,
            },
            Arc::new(TcpTransport),
            resolver.clone(),
        );
        let response = client
            .send(OutboundRequest::get(format!(
                "http://pinned.example.test:{port}/first"
            )))
            .await
            .expect("the same-host redirect is followed");
        assert_eq!(response.hops, 2);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2, "each hop is a fresh connection");
        assert!(requests[0].starts_with("GET /first HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("GET /second HTTP/1.1\r\n"));
        assert_eq!(resolver.calls(), 2);
    }

    /// B1 over a real socket: one endless header line is refused as a protocol
    /// error without reading it, so the request finishes well inside the
    /// budget. `read_until` would read until the timeout fired.
    #[tokio::test]
    async fn the_real_transport_refuses_an_endless_header_line() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nX-Endless: ").unwrap();
            let chunk = [b'x'; 8192];
            while stream.write_all(&chunk).is_ok() {}
        });
        let resolver = Arc::new(SequenceResolver::new(vec![vec![
            "127.0.0.1".parse().unwrap(),
        ]]));
        let client = OutboundClient::new(
            EgressPolicy::new(&[format!("endless.example.test:{port}")], true, false).unwrap(),
            Limits {
                timeout: Duration::from_secs(2),
                max_response_bytes: 1024,
                max_request_bytes: 1024,
                max_redirects: 2,
            },
            Arc::new(TcpTransport),
            resolver,
        );
        let started = Instant::now();
        let error = client
            .send(OutboundRequest::get(format!(
                "http://endless.example.test:{port}/"
            )))
            .await
            .expect_err("an endless header line is refused");
        assert_eq!(
            error,
            OutboundError::Transport,
            "a missing line end is a protocol error, not a timeout"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the line bound must stop the read, not the deadline: {:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }

    /// M1: the blocking reader honours the same deadline as the caller, so the
    /// peer sees the connection close when the request returns.
    #[tokio::test]
    async fn the_request_deadline_stops_the_blocking_reader() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if stream.write_all(b"HTTP/1.1 200 OK\r\n").is_err() {
                let _ = closed_tx.send(Some(Instant::now()));
                return;
            }
            let stop = Instant::now() + Duration::from_secs(5);
            while Instant::now() < stop {
                if stream.write_all(b"X").is_err() {
                    let _ = closed_tx.send(Some(Instant::now()));
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = closed_tx.send(None);
        });
        let resolver = Arc::new(SequenceResolver::new(vec![vec![
            "127.0.0.1".parse().unwrap(),
        ]]));
        let client = OutboundClient::new(
            EgressPolicy::new(&[format!("slow.example.test:{port}")], true, false).unwrap(),
            Limits {
                timeout: Duration::from_millis(200),
                max_response_bytes: 1024,
                max_request_bytes: 1024,
                max_redirects: 2,
            },
            Arc::new(TcpTransport),
            resolver,
        );
        let started = Instant::now();
        let error = client
            .send(OutboundRequest::get(format!(
                "http://slow.example.test:{port}/"
            )))
            .await
            .expect_err("a dribbling peer is refused at the deadline");
        assert!(matches!(error, OutboundError::Timeout { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the caller returns at the deadline: {:?}",
            started.elapsed()
        );
        let closed = closed_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the peer must see the connection close at the deadline");
        assert!(
            closed.is_some(),
            "the blocking reader kept the socket alive past the deadline"
        );
        server.join().unwrap();
    }

    #[test]
    fn the_client_is_shareable_across_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OutboundClient>();
    }
}
