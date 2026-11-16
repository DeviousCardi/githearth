//! The small DNS TXT lookup `doctor` needs for the public-host baseline
//! (M4.2, case U30b). It is deliberately synchronous and used only by the
//! preflight command: no request path resolves mail records.
//!
//! A resolver is a trait so the doctor check can be tested against fixed
//! answers without a network or a live DNS zone.

use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, UdpSocket},
    time::Duration,
};

/// One TXT lookup. `Err` is a transport or protocol failure, not "no record":
/// a missing record is an empty `Ok` vector.
pub trait TxtResolver {
    fn txt(&self, name: &str) -> Result<Vec<String>, String>;
}

/// The host's resolver from `/etc/resolv.conf`, with `127.0.0.1` as the
/// fallback. UDP first; a truncated answer is retried over TCP.
pub struct SystemResolver;

impl TxtResolver for SystemResolver {
    fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        let servers = nameservers();
        let mut last = "no nameserver answered".to_owned();
        for server in servers {
            match query_udp(server, name) {
                Ok((answers, truncated)) => {
                    if !truncated {
                        return Ok(answers);
                    }
                    match query_tcp(server, name) {
                        Ok(answers) => return Ok(answers),
                        Err(error) => last = error,
                    }
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

fn nameservers() -> Vec<SocketAddr> {
    let mut servers = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("nameserver")
                && let Ok(ip) = rest.trim().parse::<IpAddr>()
            {
                servers.push(SocketAddr::new(ip, 53));
            }
        }
    }
    if servers.is_empty() {
        servers.push("127.0.0.1:53".parse().expect("literal address"));
    }
    servers
}

fn build_query(name: &str) -> Result<Vec<u8>, String> {
    let mut message = Vec::with_capacity(64);
    message.extend_from_slice(&0x4d34u16.to_be_bytes()); // fixed id, local query
    message.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    message.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    message.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("invalid DNS name {name:?}"));
        }
        message.push(label.len() as u8);
        message.extend_from_slice(label.as_bytes());
    }
    message.push(0);
    message.extend_from_slice(&16u16.to_be_bytes()); // TXT
    message.extend_from_slice(&1u16.to_be_bytes()); // IN
    Ok(message)
}

fn query_udp(server: SocketAddr, name: &str) -> Result<(Vec<String>, bool), String> {
    let query = build_query(name)?;
    let socket =
        UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("cannot open a DNS socket: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("cannot set the DNS timeout: {e}"))?;
    socket
        .connect(server)
        .map_err(|e| format!("cannot reach DNS server {server}: {e}"))?;
    socket
        .send(&query)
        .map_err(|e| format!("cannot send the DNS query: {e}"))?;
    let mut response = [0u8; 4096];
    let read = socket
        .recv(&mut response)
        .map_err(|e| format!("DNS server {server} did not answer: {e}"))?;
    parse_response(&response[..read])
}

fn query_tcp(server: SocketAddr, name: &str) -> Result<Vec<String>, String> {
    let query = build_query(name)?;
    let mut stream = TcpStream::connect_timeout(&server, Duration::from_secs(2))
        .map_err(|e| format!("cannot reach DNS server {server}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let length = u16::try_from(query.len()).map_err(|_| "DNS query is too large".to_owned())?;
    stream
        .write_all(&length.to_be_bytes())
        .and_then(|()| stream.write_all(&query))
        .map_err(|e| format!("cannot send the DNS query: {e}"))?;
    let mut size = [0u8; 2];
    stream
        .read_exact(&mut size)
        .map_err(|e| format!("DNS server {server} did not answer: {e}"))?;
    let size = usize::from(u16::from_be_bytes(size));
    if size > 65_535 {
        return Err("DNS response is too large".to_owned());
    }
    let mut response = vec![0u8; size];
    stream
        .read_exact(&mut response)
        .map_err(|e| format!("cannot read the DNS response: {e}"))?;
    parse_response(&response).map(|(answers, _)| answers)
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

/// Skip a possibly compressed name. Returns the offset just past it.
fn skip_name(bytes: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let length = *bytes.get(offset)?;
        if length == 0 {
            return Some(offset + 1);
        }
        if length & 0xc0 == 0xc0 {
            return Some(offset + 2);
        }
        offset = offset.checked_add(1 + usize::from(length))?;
        if offset > bytes.len() {
            return None;
        }
    }
}

fn parse_response(bytes: &[u8]) -> Result<(Vec<String>, bool), String> {
    if bytes.len() < 12 {
        return Err("DNS response is too short".to_owned());
    }
    let flags = u16_at(bytes, 2).ok_or("DNS response header is truncated")?;
    if flags & 0x8000 == 0 {
        return Err("DNS response is not an answer".to_owned());
    }
    let rcode = flags & 0x000f;
    let truncated = flags & 0x0200 != 0;
    // NXDOMAIN is an answer about the name, not an outage: the name does not
    // exist, so it publishes no TXT record. `doctor` must report a missing
    // record for a typo'd sending domain instead of turning it into a warning.
    if rcode == 3 {
        return Ok((Vec::new(), truncated));
    }
    if rcode != 0 {
        return Err(format!("DNS server returned error code {rcode}"));
    }
    let questions = u16_at(bytes, 4).ok_or("DNS response header is truncated")?;
    let answers = u16_at(bytes, 6).ok_or("DNS response header is truncated")?;
    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(bytes, offset).ok_or("DNS question name is truncated")?;
        offset = offset.checked_add(4).ok_or("DNS question is truncated")?;
        if offset > bytes.len() {
            return Err("DNS question is truncated".to_owned());
        }
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        offset = skip_name(bytes, offset).ok_or("DNS answer name is truncated")?;
        let kind = u16_at(bytes, offset).ok_or("DNS answer is truncated")?;
        let length = usize::from(u16_at(bytes, offset + 8).ok_or("DNS answer is truncated")?);
        let start = offset.checked_add(10).ok_or("DNS answer is truncated")?;
        let end = start.checked_add(length).ok_or("DNS answer is truncated")?;
        if end > bytes.len() {
            return Err("DNS answer is truncated".to_owned());
        }
        if kind == 16 {
            let mut text = String::new();
            let mut cursor = start;
            while cursor < end {
                let part_length = usize::from(bytes[cursor]);
                cursor += 1;
                let part_end = cursor
                    .checked_add(part_length)
                    .ok_or("DNS TXT record is truncated")?;
                if part_end > end {
                    return Err("DNS TXT record is truncated".to_owned());
                }
                text.push_str(&String::from_utf8_lossy(&bytes[cursor..part_end]));
                cursor = part_end;
            }
            records.push(text);
        }
        offset = end;
    }
    Ok((records, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_txt_query_has_the_question_and_type() {
        let query = build_query("_dmarc.example.com").unwrap();
        assert_eq!(&query[12..13], &[6]);
        assert!(query.ends_with(&[0, 16, 0, 1]));
    }

    #[test]
    fn a_txt_answer_is_parsed_with_compression() {
        // Header: id, flags QR|RD, 1 question, 1 answer.
        let mut message = vec![
            0x00, 0x01, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        message.extend_from_slice(&[7]);
        message.extend_from_slice(b"example");
        message.extend_from_slice(&[3]);
        message.extend_from_slice(b"com");
        message.push(0);
        message.extend_from_slice(&[0, 16, 0, 1]);
        // Answer name: pointer to offset 12; type TXT; class IN; ttl; rdlength.
        message.extend_from_slice(&[0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60]);
        let txt = b"v=spf1 -all";
        message.extend_from_slice(&[0, (txt.len() + 1) as u8, txt.len() as u8]);
        message.extend_from_slice(txt);
        let (records, truncated) = parse_response(&message).unwrap();
        assert!(!truncated);
        assert_eq!(records, vec!["v=spf1 -all"]);
    }

    #[test]
    fn a_truncated_flag_is_reported() {
        let message = vec![
            0x00, 0x01, 0x83, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let (records, truncated) = parse_response(&message).unwrap();
        assert!(records.is_empty());
        assert!(truncated);
    }

    #[test]
    fn nxdomain_is_an_empty_answer_not_an_outage() {
        // QR|RD|RA with RCODE 3 (NXDOMAIN) and no answers.
        let message = vec![
            0x00, 0x01, 0x81, 0x83, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let (records, truncated) = parse_response(&message).expect("NXDOMAIN is an answer");
        assert!(records.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn servfail_is_still_an_outage() {
        // RCODE 2 (SERVFAIL) must stay an error: the record may exist.
        let message = vec![
            0x00, 0x01, 0x81, 0x82, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(parse_response(&message).is_err());
    }

    #[test]
    fn a_truncated_record_is_an_error_not_an_empty_answer() {
        let message = vec![
            0x00, 0x01, 0x81, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(parse_response(&message).is_err());
    }
}
