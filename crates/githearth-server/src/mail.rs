//! Outbound mail (M4.1 U08).
//!
//! One seam, [`MailTransport`], is all the email handlers know. `serve` builds
//! [`SmtpTransport`] from configuration; tests inject a memory transport and
//! never open a socket. The SMTP credential is read from a file (or stdin) at
//! start-up, kept in this module, and never formatted: [`MailError`] has fixed
//! text and no inner error, so a driver message cannot leak a password through
//! a log line, a job's `last_error` or an HTTP body.

use crate::config::{Config, SmtpTls};
use base64::Engine;
use std::{
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// One plain-text message. No HTML, no attachments, no headers supplied by the
/// caller: every header is written here from validated fields.
#[derive(Clone, PartialEq, Eq)]
pub struct EmailMessage {
    pub to: String,
    pub subject: String,
    pub body: String,
    /// The stable identity written as the `Message-ID` local part: the email
    /// intent's id, or the verification row's id. A retry of the same intent or
    /// request reuses it, so a receiving server that deduplicates on
    /// `Message-ID` sees one message; a new request gets a new row and so a new
    /// id. It is not the timestamp: the `Date:` header is stamped per attempt.
    pub id: Uuid,
}

impl std::fmt::Debug for EmailMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The body never carries a credential, but a destination address is
        // personal data; a debug line should not become a copy of it.
        f.debug_struct("EmailMessage")
            .field("to", &"[redacted]")
            .field("subject", &self.subject)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A send failure. Deliberately carries no inner error: the SMTP driver's text
/// can name the host or a rejected address, and a future driver could echo a
/// credential. Handlers log and persist only this type's fixed `Display`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailError {
    Disabled,
    Connect,
    Tls,
    Protocol,
    Auth,
    Rejected,
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "email delivery is not configured",
            Self::Connect => "cannot connect to the SMTP server",
            Self::Tls => "SMTP TLS negotiation failed",
            Self::Protocol => "SMTP conversation failed",
            Self::Auth => "SMTP authentication failed",
            Self::Rejected => "SMTP server rejected the message",
        })
    }
}

pub type MailFuture<'a> = Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

/// The injection seam. `send` is called from a durable job, never from a
/// request handler.
pub trait MailTransport: Send + Sync {
    fn send<'a>(&'a self, message: &'a EmailMessage) -> MailFuture<'a>;
}

/// The default when `smtp_host` is empty: no socket, no delivery. A queued
/// intent stays queued (and retries) until an operator configures SMTP.
pub struct DisabledTransport;

impl MailTransport for DisabledTransport {
    fn send<'a>(&'a self, _message: &'a EmailMessage) -> MailFuture<'a> {
        Box::pin(async { Err(MailError::Disabled) })
    }
}

/// A deterministic in-memory transport for tests. It records only successful
/// deliveries separately from attempts, so a test can assert both "retried"
/// and "delivered once".
#[derive(Default)]
pub struct MemoryTransport {
    delivered: Mutex<Vec<EmailMessage>>,
    attempts: AtomicUsize,
    fail_next: AtomicUsize,
}

impl MemoryTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail the next `n` sends. A failed attempt is counted but not delivered.
    pub fn fail_next(&self, n: usize) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    pub fn delivered(&self) -> Vec<EmailMessage> {
        self.delivered.lock().unwrap().clone()
    }
}

impl MailTransport for MemoryTransport {
    fn send<'a>(&'a self, message: &'a EmailMessage) -> MailFuture<'a> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let mut remaining = self.fail_next.load(Ordering::SeqCst);
            while remaining > 0 {
                match self.fail_next.compare_exchange(
                    remaining,
                    remaining - 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => return Err(MailError::Connect),
                    Err(actual) => remaining = actual,
                }
            }
            self.delivered.lock().unwrap().push(message.clone());
            Ok(())
        })
    }
}

/// `serve`'s transport. A missing configuration is [`DisabledTransport`], not
/// an error: an installation may run without mail. A configured installation
/// with an unreadable credential file fails at start-up, before serving.
pub fn transport_from_config(config: &Config) -> anyhow::Result<Arc<dyn MailTransport>> {
    if config.smtp_host.is_empty() {
        return Ok(Arc::new(DisabledTransport));
    }
    let password = match &config.smtp_password_file {
        None => None,
        Some(path) if path == Path::new("-") => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|_| anyhow::anyhow!("cannot read the SMTP credential from stdin"))?;
            Some(trim_credential(&line))
        }
        Some(path) => {
            let value = std::fs::read_to_string(path)
                // Path only: the file's contents must never reach this message.
                .map_err(|_| anyhow::anyhow!("cannot read the SMTP credential file"))?;
            Some(trim_credential(&value))
        }
    };
    let envelope_from = if config.smtp_bounce_address.trim().is_empty() {
        config.smtp_from.clone()
    } else {
        config.smtp_bounce_address.clone()
    };
    Ok(Arc::new(SmtpTransport {
        host: config.smtp_host.clone(),
        port: config.smtp_port,
        tls: config.smtp_tls,
        from: config.smtp_from.clone(),
        envelope_from,
        username: config.smtp_username.clone(),
        password,
        allow_private: config.outbound_allow_private,
    }))
}

fn trim_credential(value: &str) -> String {
    value.trim_end_matches(['\r', '\n']).to_string()
}

/// A blocking SMTP client run on the blocking pool. It supports STARTTLS,
/// implicit TLS and cleartext (the last for a loopback development relay).
pub struct SmtpTransport {
    host: String,
    port: u16,
    tls: SmtpTls,
    from: String,
    /// The envelope sender (`MAIL FROM`) bounces go to. Defaults to `from`.
    envelope_from: String,
    username: Option<String>,
    password: Option<String>,
    /// The installation-wide private-range opt-in. The connect uses the same
    /// address policy as the outbound HTTP client, so a metadata endpoint is
    /// never contacted and loopback/private answers need this switch.
    allow_private: bool,
}

impl std::fmt::Debug for SmtpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpTransport")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("from", &self.from)
            .field("envelope_from", &self.envelope_from)
            .field("username", &self.username.as_deref().map(|_| "[redacted]"))
            .field("password", &self.password.as_deref().map(|_| "[redacted]"))
            .finish()
    }
}

impl MailTransport for SmtpTransport {
    fn send<'a>(&'a self, message: &'a EmailMessage) -> MailFuture<'a> {
        Box::pin(async move {
            let message = message.clone();
            let settings = Settings {
                host: self.host.clone(),
                port: self.port,
                tls: self.tls,
                from: self.from.clone(),
                envelope_from: self.envelope_from.clone(),
                username: self.username.clone(),
                password: self.password.clone(),
                allow_private: self.allow_private,
            };
            tokio::task::spawn_blocking(move || send_blocking(&settings, &message))
                .await
                .map_err(|_| MailError::Protocol)?
        })
    }
}

#[derive(Clone)]
struct Settings {
    host: String,
    port: u16,
    tls: SmtpTls,
    from: String,
    envelope_from: String,
    username: Option<String>,
    password: Option<String>,
    allow_private: bool,
}

fn send_blocking(settings: &Settings, message: &EmailMessage) -> Result<(), MailError> {
    // SMTP is not HTTP, so it cannot use the outbound client's transport, but
    // it uses the same resolution and address policy: resolve once, refuse a
    // metadata answer (and a private one without the opt-in), then connect to
    // the returned address. The name is never handed to the socket layer.
    let address = crate::outbound::resolve_operator_destination(
        &settings.host,
        settings.port,
        settings.allow_private,
    )
    .map_err(|reason| {
        tracing::warn!(target: "outbound", %reason, "SMTP destination refused");
        MailError::Connect
    })?;
    // outbound-exempt: SMTP connects to the one address the shared policy checked and pinned
    let tcp = TcpStream::connect_timeout(&address, Duration::from_secs(30))
        .map_err(|_| MailError::Connect)?;
    tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(30))).ok();
    match settings.tls {
        SmtpTls::None => {
            let mut stream = BufReader::new(tcp);
            conversation(&mut stream, settings, message, true, true)
        }
        SmtpTls::Implicit => {
            let mut stream = BufReader::new(tls_stream(tcp, &settings.host)?);
            conversation(&mut stream, settings, message, true, true)
        }
        SmtpTls::Starttls => {
            let mut stream = BufReader::new(tcp);
            read_greeting(&mut stream)?;
            expect(&mut stream, "EHLO localhost", 250)?;
            expect(&mut stream, "STARTTLS", 220)?;
            let tcp = stream.into_inner();
            let mut stream = BufReader::new(tls_stream(tcp, &settings.host)?);
            conversation(&mut stream, settings, message, false, true)
        }
    }
}

fn tls_stream(
    tcp: TcpStream,
    host: &str,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, MailError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name =
        rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| MailError::Tls)?;
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|_| MailError::Tls)?;
    Ok(rustls::StreamOwned::new(connection, tcp))
}

/// The post-greeting half of the conversation. `greet` is false after a TLS
/// handshake that followed a greeting on the same connection.
fn conversation<S: Read + Write>(
    stream: &mut BufReader<S>,
    settings: &Settings,
    message: &EmailMessage,
    greet: bool,
    ehlo: bool,
) -> Result<(), MailError> {
    if greet {
        read_greeting(stream)?;
    }
    if ehlo {
        let (code, _) = command(stream, "EHLO localhost")?;
        if code != 250 {
            return Err(MailError::Protocol);
        }
    }
    if let (Some(username), Some(password)) = (&settings.username, &settings.password) {
        let plain =
            base64::engine::general_purpose::STANDARD.encode(format!("\0{username}\0{password}"));
        let (code, text) = command(stream, &format!("AUTH PLAIN {plain}"))?;
        if code == 235 {
            // Authenticated.
        } else if code == 504 || code == 500 {
            // Fall back to the older AUTH LOGIN exchange.
            expect(stream, "AUTH LOGIN", 334)?;
            expect(
                stream,
                &base64::engine::general_purpose::STANDARD.encode(username),
                334,
            )?;
            expect(
                stream,
                &base64::engine::general_purpose::STANDARD.encode(password),
                235,
            )?;
        } else {
            let _ = text;
            return Err(MailError::Auth);
        }
    }
    expect(
        stream,
        &format!("MAIL FROM:<{}>", settings.envelope_from),
        250,
    )?;
    // `expect` also accepts 251, which some relays answer for a forwarded
    // recipient.
    expect(stream, &format!("RCPT TO:<{}>", message.to), 250)?;
    expect(stream, "DATA", 354)?;
    write_data(stream, settings, message)?;
    let (code, _) = command(stream, "")?;
    if code != 250 {
        return Err(MailError::Rejected);
    }
    let _ = command(stream, "QUIT");
    Ok(())
}

fn read_greeting<S: Read + Write>(stream: &mut BufReader<S>) -> Result<(), MailError> {
    let (code, _) = read_reply(stream)?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(MailError::Protocol)
    }
}

fn expect<S: Read + Write>(
    stream: &mut BufReader<S>,
    line: &str,
    want: u16,
) -> Result<(), MailError> {
    let (code, _) = command(stream, line)?;
    if code == want || (line.starts_with("RCPT TO") && code == 251) {
        Ok(())
    } else {
        Err(MailError::Rejected)
    }
}

fn command<S: Read + Write>(
    stream: &mut BufReader<S>,
    line: &str,
) -> Result<(u16, String), MailError> {
    // Every caller-supplied value is validated before it reaches here: the
    // address format forbids control bytes and the subject is built from safe
    // pieces, but strip CR/LF anyway so no field can inject a command.
    let line = line.replace(['\r', '\n'], "");
    if !line.is_empty() {
        stream
            .get_mut()
            .write_all(format!("{line}\r\n").as_bytes())
            .map_err(|_| MailError::Protocol)?;
        stream.get_mut().flush().map_err(|_| MailError::Protocol)?;
    }
    read_reply(stream)
}

fn read_reply<S: Read + Write>(stream: &mut BufReader<S>) -> Result<(u16, String), MailError> {
    let mut text = String::new();
    loop {
        let mut line = String::new();
        let read = stream
            .read_line(&mut line)
            .map_err(|_| MailError::Protocol)?;
        if read == 0 {
            return Err(MailError::Protocol);
        }
        if line.len() < 3 {
            return Err(MailError::Protocol);
        }
        let code = line[..3].parse::<u16>().map_err(|_| MailError::Protocol)?;
        text.push_str(&line);
        // A hyphen after the code means another line follows.
        if line.as_bytes().get(3) != Some(&b'-') {
            return Ok((code, text));
        }
    }
}

fn write_data<S: Read + Write>(
    stream: &mut BufReader<S>,
    settings: &Settings,
    message: &EmailMessage,
) -> Result<(), MailError> {
    // `Date:` is stamped when the message is written, so a retry carries the
    // time it was actually sent. `Message-ID:` is derived from the durable
    // intent/row id in `message.id`, so every retry of the same intent or
    // request carries the same id and a receiving server can deduplicate it;
    // only a new intent or verification request gets a new id.
    let domain = settings
        .from
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| !domain.is_empty())
        .unwrap_or("githearth.invalid");
    let headers = format!(
        "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{}@{}>\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
        header(settings.from.as_str()),
        header(message.to.as_str()),
        header(message.subject.as_str()),
        rfc5322_date(SystemTime::now()),
        message.id,
        header(domain),
    );
    stream
        .get_mut()
        .write_all(headers.as_bytes())
        .map_err(|_| MailError::Protocol)?;
    for line in message.body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('.') {
            stream
                .get_mut()
                .write_all(b".")
                .map_err(|_| MailError::Protocol)?;
        }
        stream
            .get_mut()
            .write_all(line.as_bytes())
            .and_then(|()| stream.get_mut().write_all(b"\r\n"))
            .map_err(|_| MailError::Protocol)?;
    }
    stream
        .get_mut()
        .write_all(b".\r\n")
        .and_then(|()| stream.get_mut().flush())
        .map_err(|_| MailError::Protocol)
}

fn header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// An RFC 5322 `date-time` in UTC: `Sun, 15 Nov 2026 12:34:56 +0000`. Written
/// by hand rather than adding a date crate for one header; the conversion is
/// Howard Hinnant's `civil_from_days`, and the unit test pins the epoch, a leap
/// day and a known weekday.
fn rfc5322_date(now: SystemTime) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let clock = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        WEEKDAYS[days.rem_euclid(7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        clock / 3_600,
        (clock % 3_600) / 60,
        clock % 60,
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_is_not_in_a_transport_debug_or_an_error() {
        let transport = SmtpTransport {
            host: "smtp.example.com".into(),
            port: 587,
            tls: SmtpTls::Starttls,
            from: "githearth@example.com".into(),
            envelope_from: "bounces@example.com".into(),
            username: Some("githearth".into()),
            password: Some("smtp-credential-must-not-appear".into()),
            allow_private: false,
        };
        let debug = format!("{transport:?}");
        assert!(
            !debug.contains("smtp-credential-must-not-appear"),
            "{debug}"
        );
        for error in [
            MailError::Disabled,
            MailError::Connect,
            MailError::Tls,
            MailError::Protocol,
            MailError::Auth,
            MailError::Rejected,
        ] {
            assert!(!format!("{error} {error:?}").contains("smtp-credential"));
        }
    }

    #[tokio::test]
    async fn the_memory_transport_counts_attempts_separately_from_deliveries() {
        let transport = MemoryTransport::new();
        transport.fail_next(1);
        let message = EmailMessage {
            to: "a@example.com".into(),
            subject: "s".into(),
            body: "b".into(),
            id: Uuid::new_v4(),
        };
        assert_eq!(transport.send(&message).await, Err(MailError::Connect));
        assert_eq!(transport.send(&message).await, Ok(()));
        assert_eq!(transport.attempts(), 2);
        assert_eq!(transport.delivered(), vec![message]);
    }

    #[test]
    fn the_date_header_is_an_rfc5322_utc_date() {
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(rfc5322_date(at(0)), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc5322_date(at(1_709_164_800)),
            "Thu, 29 Feb 2024 00:00:00 +0000",
            "a leap day"
        );
        assert_eq!(
            rfc5322_date(at(1_794_746_096)),
            "Sun, 15 Nov 2026 12:34:56 +0000"
        );
    }

    #[test]
    fn a_message_carries_a_date_and_its_stable_message_id() {
        let settings = Settings {
            host: "smtp.example.com".into(),
            port: 587,
            tls: SmtpTls::Starttls,
            from: "githearth@example.com".into(),
            envelope_from: "bounce@example.com".into(),
            username: None,
            password: None,
            allow_private: false,
        };
        let id = Uuid::new_v4();
        let message = EmailMessage {
            to: "a@example.com".into(),
            subject: "s".into(),
            body: "b".into(),
            id,
        };
        let mut stream = BufReader::new(std::io::Cursor::new(Vec::new()));
        write_data(&mut stream, &settings, &message).unwrap();
        let written = String::from_utf8(stream.into_inner().into_inner()).unwrap();
        // Revert: remove the `Date:` and `Message-ID:` headers from
        // `write_data`; these assertions fail.
        assert!(
            written.contains(&format!("Message-ID: <{id}@example.com>\r\n")),
            "{written}"
        );
        assert!(written.contains("\r\nDate: "), "{written}");
        assert!(written.starts_with("From: githearth@example.com\r\n"));
    }
}
