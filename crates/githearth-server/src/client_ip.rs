use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The resolved client IP, inserted as a request extension by the `ClientIpLayer`.
/// Public so callers such as the audit system can read it (TASK #88).
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

/// A single trusted-proxy entry: an individual IP or a CIDR range.
#[derive(Debug, Clone)]
pub(crate) enum TrustedProxy {
    Single(IpAddr),
    Cidr { addr: IpAddr, prefix_len: u8 },
}

/// Parse one entry from the config list.  Returns `Err(())` on unparseable input.
pub(crate) fn parse_trusted_proxy(s: &str) -> Result<TrustedProxy, ()> {
    if let Some((addr_str, len_str)) = s.split_once('/') {
        let prefix_len: u8 = len_str.parse().map_err(|_| ())?;
        let addr: IpAddr = addr_str.parse().map_err(|_| ())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix_len > max {
            return Err(());
        }
        Ok(TrustedProxy::Cidr { addr, prefix_len })
    } else {
        let addr: IpAddr = s.parse().map_err(|_| ())?;
        Ok(TrustedProxy::Single(addr))
    }
}

/// Returns `true` when `ip` is inside any of the trusted entries.
fn is_trusted(ip: IpAddr, trusted: &[TrustedProxy]) -> bool {
    trusted.iter().any(|t| match t {
        TrustedProxy::Single(a) => ip == *a,
        TrustedProxy::Cidr { addr, prefix_len } => {
            let pl = *prefix_len;
            match (ip, addr) {
                (IpAddr::V4(client), IpAddr::V4(network)) => {
                    if pl > 32 {
                        return false;
                    }
                    subnet_match_v4(client, *network, pl)
                }
                (IpAddr::V6(client), IpAddr::V6(network)) => {
                    if pl > 128 {
                        return false;
                    }
                    subnet_match_v6(client, *network, pl)
                }
                _ => false,
            }
        }
    })
}

fn subnet_match_v4(client: Ipv4Addr, network: Ipv4Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    let mask = u32::MAX.checked_shl(32 - prefix_len as u32).unwrap_or(0);
    let client_bits = u32::from(client);
    let network_bits = u32::from(network);
    (client_bits & mask) == (network_bits & mask)
}

fn subnet_match_v6(client: Ipv6Addr, network: Ipv6Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    let mask = u128::MAX.checked_shl(128 - prefix_len as u32).unwrap_or(0);
    let client_bits = u128::from(client);
    let network_bits = u128::from(network);
    (client_bits & mask) == (network_bits & mask)
}

/// Resolve the true client IP given the TCP peer address and the `X-Forwarded-For`
/// header values (in order).  The header is only consulted when the peer itself is
/// trusted; otherwise the peer is returned directly so a client cannot spoof its IP.
///
/// When the peer is trusted the function walks the header from right to left,
/// skipping entries that are also trusted proxies.  The first untrusted entry is
/// the client.  If the header is empty, all entries are trusted, or any entry is
/// malformed, the peer address is returned — we never trust an unparseable value.
///
/// `X-Real-IP` and the structured `Forwarded` header are not implemented; out of
/// scope per TASK #88.
pub(crate) fn client_ip(
    peer: SocketAddr,
    forwarded: &[String],
    trusted: &[TrustedProxy],
) -> IpAddr {
    let peer_ip = peer.ip();
    if trusted.is_empty() || !is_trusted(peer_ip, trusted) {
        return normalise(peer_ip);
    }
    let mut candidates: Vec<IpAddr> = Vec::new();
    for header_value in forwarded {
        for part in header_value.split(',') {
            let part = part.trim();
            if let Ok(ip) = part.parse::<IpAddr>() {
                candidates.push(ip);
            } else {
                return normalise(peer_ip); // malformed entry → fall back
            }
        }
    }
    for ip in candidates.into_iter().rev() {
        if !is_trusted(ip, trusted) {
            return normalise(ip);
        }
    }
    normalise(peer_ip) // all-trusted chain → fall back
}

/// Collapse an IPv4-mapped IPv6 address (`::ffff:203.0.113.7`) to its IPv4 form
/// (`203.0.113.7`) so the audit digest is identical for both (TASK #105). Any
/// other address is returned unchanged.
fn normalise(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted_list(entries: &[&str]) -> Vec<TrustedProxy> {
        entries
            .iter()
            .map(|s| parse_trusted_proxy(s).unwrap())
            .collect()
    }

    fn p(ip: &str) -> SocketAddr {
        ip.parse::<IpAddr>().unwrap();
        format!("{ip}:12345")
            .parse::<SocketAddr>()
            .unwrap_or_else(|_| format!("[{ip}]:12345").parse::<SocketAddr>().unwrap())
    }

    #[test]
    fn parse_single_v4() {
        let t = parse_trusted_proxy("10.0.0.1").unwrap();
        assert!(matches!(t, TrustedProxy::Single(IpAddr::V4(..))));
    }

    #[test]
    fn parse_cidr_v4() {
        let t = parse_trusted_proxy("10.0.0.0/24").unwrap();
        assert!(matches!(t, TrustedProxy::Cidr { .. }));
    }

    #[test]
    fn parse_cidr_v6() {
        let t = parse_trusted_proxy("::1/128").unwrap();
        assert!(matches!(t, TrustedProxy::Cidr { .. }));
    }

    #[test]
    fn parse_invalid_bits() {
        assert!(parse_trusted_proxy("10.0.0.0/33").is_err());
        assert!(parse_trusted_proxy("::1/129").is_err());
    }

    #[test]
    fn parse_empty_rejected() {
        assert!(parse_trusted_proxy("").is_err());
    }

    #[test]
    fn parse_garbage_rejected() {
        assert!(parse_trusted_proxy("not-an-ip").is_err());
        assert!(parse_trusted_proxy("10.0.0.0/abc").is_err());
    }

    #[test]
    fn untrusted_peer_ignores_header() {
        let peer = p("203.0.113.5");
        let fwd = vec!["1.2.3.4".into()];
        let trusted = trusted_list(&[]);
        assert_eq!(client_ip(peer, &fwd, &trusted), peer.ip());

        let trusted = trusted_list(&["10.0.0.1"]);
        assert_eq!(client_ip(peer, &fwd, &trusted), peer.ip());
    }

    #[test]
    fn trusted_peer_takes_header() {
        let peer = p("10.0.0.1");
        let fwd = vec!["203.0.113.5".into()];
        let trusted = trusted_list(&["10.0.0.1"]);

        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn rightmost_untrusted_wins() {
        let peer = p("10.0.0.1");
        let fwd = vec!["203.0.113.5, 10.0.0.2".into()];
        let trusted = trusted_list(&["10.0.0.1", "10.0.0.2"]);
        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn chain_of_proxies() {
        let peer = p("10.0.0.3");
        let fwd = vec!["203.0.113.5, 10.0.0.1, 10.0.0.2".into()];
        let trusted = trusted_list(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn ipv6_client() {
        let peer = p("::1");
        let fwd = vec!["2001:db8::1".into()];
        let trusted = trusted_list(&["::1"]);
        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn malformed_header_falls_back_to_peer() {
        let peer = p("10.0.0.1");
        let fwd = vec!["not-an-ip".into()];
        let trusted = trusted_list(&["10.0.0.1"]);

        assert_eq!(client_ip(peer, &fwd, &trusted), peer.ip());
    }

    #[test]
    fn empty_header_falls_back_to_peer() {
        let peer = p("10.0.0.1");
        let fwd: Vec<String> = vec![];
        let trusted = trusted_list(&["10.0.0.1"]);

        assert_eq!(client_ip(peer, &fwd, &trusted), peer.ip());
    }

    #[test]
    fn all_trusted_chain_falls_back_to_peer() {
        let peer = p("10.0.0.2");
        let fwd = vec!["10.0.0.3, 10.0.0.4".into()];
        let trusted = trusted_list(&["10.0.0.2", "10.0.0.3", "10.0.0.4"]);
        assert_eq!(client_ip(peer, &fwd, &trusted), peer.ip());
    }

    #[test]
    fn cidr_edge_inside() {
        let peer = p("10.0.0.1");
        let fwd = vec!["10.0.0.200".into()]; // inside 10.0.0.0/24
        let trusted = trusted_list(&["10.0.0.0/24"]);
        // Both peer and header are inside the trusted CIDR → all trusted → fall back
        assert_eq!(client_ip(peer, &fwd, &trusted), peer.ip());
    }

    #[test]
    fn cidr_edge_outside() {
        let peer = p("10.0.0.1");
        let fwd = vec!["203.0.113.5, 10.0.0.200".into()];
        let trusted = trusted_list(&["10.0.0.0/24"]);
        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn multiple_forwarded_headers_concatenated() {
        let peer = p("10.0.0.1");
        let fwd = vec!["203.0.113.5".into(), "10.0.0.2".into()];
        let trusted = trusted_list(&["10.0.0.1", "10.0.0.2"]);
        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn normalise_mapped_to_ipv4() {
        assert_eq!(
            normalise("::ffff:203.0.113.7".parse().unwrap()),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn normalise_plain_ipv6_unchanged() {
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(normalise(v6), v6);
    }

    #[test]
    fn normalise_ipv4_unchanged() {
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(normalise(v4), v4);
    }

    #[test]
    fn mapped_header_normalises_to_ipv4() {
        let peer = p("10.0.0.1");
        let fwd = vec!["::ffff:203.0.113.7".into()];
        let trusted = trusted_list(&["10.0.0.1"]);
        assert_eq!(
            client_ip(peer, &fwd, &trusted),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }
}
