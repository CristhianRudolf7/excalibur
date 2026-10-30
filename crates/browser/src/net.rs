//! Loopback egress ownership, document origin sets and navigation exfiltration checks.
//!
//! The loopback listener only accepts TCP peers owned by the web user. Document
//! gates compare committed frame origins, never raw strings, and navigation
//! risks are reported deterministically without any model judgement.
use std::net::SocketAddr;
use std::str::FromStr;

/// Forbidden top-level schemes. Navigation to these is always denied.
pub const FORBIDDEN_SCHEMES: &[&str] = &[
    "file",
    "chrome",
    "chrome-extension",
    "devtools",
    "javascript",
    "data",
];

/// Parse one `/proc/net/tcp` style table into (local, remote, inode, uid).
/// Each data line is expected to contain local/remote hex addresses, state and
/// `tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode` tail fields.
pub fn parse_proc_tcp_table(content: &str) -> Vec<(SocketAddr, SocketAddr, u32, u32)> {
    let mut out = Vec::new();
    for line in content.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 10 {
            continue;
        }
        let local = parse_hex_addr(parts[1]);
        let remote = parse_hex_addr(parts[2]);
        let uid: Option<u32> = parts[7].parse().ok();
        let inode: Option<u32> = parts[9].parse().ok();
        if let (Some(local), Some(remote), Some(uid), Some(inode)) = (local, remote, uid, inode) {
            out.push((local, remote, inode, uid));
        }
    }
    out
}

fn parse_hex_addr(raw: &str) -> Option<SocketAddr> {
    let (ip_hex, port_hex) = raw.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    if ip_hex.len() == 8 {
        let raw = u32::from_str_radix(ip_hex, 16).ok()?;
        // /proc/net/tcp stores IPv4 in little-endian host order.
        let bytes = raw.to_le_bytes();
        let ip = std::net::Ipv4Addr::from(bytes);
        Some(SocketAddr::new(ip.into(), port))
    } else if ip_hex.len() == 32 {
        let mut bytes = [0u8; 16];
        for (i, chunk) in bytes.iter_mut().enumerate() {
            let part = ip_hex.get(i * 2..i * 2 + 2)?;
            *chunk = u8::from_str_radix(part, 16).ok()?;
        }
        // Kernel prints each 32-bit word in host order; reverse per word so the
        // lookup matches the real peer address on little-endian machines.
        let (words, _) = bytes.as_chunks_mut::<4>();
        for word in words {
            word.reverse();
        }
        let ip = std::net::Ipv6Addr::from(bytes);
        Some(SocketAddr::new(ip.into(), port))
    } else {
        None
    }
}

/// Check that a loopback TCP peer is owned by `expected_uid`.
///
/// `tcp` and `tcp6` are the full contents of `/proc/net/tcp` and
/// `/proc/net/tcp6`. The peer must appear as the exact (local, remote) tuple
/// or as the remote side of a connection whose local side is a loopback
/// address with port `listener_port`. Unknown peers fail closed.
pub fn check_loopback_uid(
    peer: SocketAddr,
    listener_port: u16,
    expected_uid: u32,
    tcp: &str,
    tcp6: &str,
) -> Result<(), String> {
    if peer.port() == 0 {
        return Err("unknown loopback peer".into());
    }
    let mut candidates = parse_proc_tcp_table(tcp);
    candidates.extend(parse_proc_tcp_table(tcp6));
    // The accepted peer is always the remote side: it must be the remote of a
    // connection whose local side is a loopback address with the listener
    // port. Anything else (including a peer equal to our own socket) fails
    // closed.
    let found = candidates.iter().find(|(local, remote, _, _)| {
        *remote == peer && local.port() == listener_port && is_loopback(local.ip())
    });
    match found {
        Some((_, _, _, uid)) if *uid == expected_uid => Ok(()),
        Some((_, _, _, uid)) => Err(format!("loopback peer owned by uid {uid}")),
        None => Err("unknown loopback peer".into()),
    }
}

fn is_loopback(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => v.octets()[0] == 127,
        std::net::IpAddr::V6(v) => v.is_loopback(),
    }
}

/// Committed HTTP(S) origin (`scheme://host[:port]`), lowercased host.
pub fn document_origin(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

/// True when `url` uses a forbidden top-level scheme.
pub fn is_forbidden_scheme(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    FORBIDDEN_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(&format!("{scheme}:")))
}

/// Shannon entropy in bits per character.
pub fn shannon_entropy(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let bytes = text.as_bytes();
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let len = bytes.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let probability = *count as f64 / len;
            -probability * probability.log2()
        })
        .sum()
}

/// Deterministic C4 check for a top-level navigation or cross-origin form send.
///
/// Returns `Some(reason)` when the run is contaminated and the destination is
/// a new origin carrying a long (>128 chars) or high-entropy query/fragment,
/// or when `method` is POST across origins. Same-origin traffic is allowed.
pub fn exfiltration_check(from_origin: Option<&str>, to_url: &str, method: &str) -> Option<String> {
    let to_origin = document_origin(to_url)?;
    if Some(to_origin.as_str()) == from_origin {
        return None;
    }
    let parsed = reqwest::Url::parse(to_url).ok()?;
    let query = parsed.query().unwrap_or("");
    let fragment = parsed.fragment().unwrap_or("");
    let long = query.len() > 128 || fragment.len() > 128;
    let entropic = (!query.is_empty() && shannon_entropy(query) > 4.5)
        || (!fragment.is_empty() && shannon_entropy(fragment) > 4.5);
    if method.eq_ignore_ascii_case("post") {
        return Some("cross_origin_post".into());
    }
    if long {
        return Some("long_query_or_fragment_to_new_origin".into());
    }
    if entropic {
        return Some("high_entropy_query_or_fragment_to_new_origin".into());
    }
    // New origin without payload is still an origin-set event, but it is not
    // an exfiltration signal on its own.
    None
}

/// Whether `url` introduces an origin outside `known` document origins.
pub fn is_new_origin(url: &str, known: &[String]) -> bool {
    match document_origin(url) {
        Some(origin) => !known.contains(&origin),
        None => false,
    }
}

pub fn parse_hex_port_for_test() -> bool {
    SocketAddr::from_str("127.0.0.1:80").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_tcp_and_matches_peer_uid() {
        // 0100007F:0035 is 127.0.0.1:53 in /proc little-endian order.
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 0100007F:0035 01 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n";
        let rows = parse_proc_tcp_table(table);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3, 1000);
        assert_eq!(rows[0].0.port(), 8080);
        let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert!(check_loopback_uid(peer, 53, 1000, table, "").is_err());
        // Exact tuple match: listener port 8080, peer 127.0.0.1:53.
        let peer2: SocketAddr = "127.0.0.1:53".parse().unwrap();
        assert!(check_loopback_uid(peer2, 8080, 1000, table, "").is_ok());
        assert!(check_loopback_uid(peer2, 8080, 999, table, "").is_err());
        assert!(check_loopback_uid(peer2, 9999, 1000, table, "").is_err());
    }

    #[test]
    fn rejects_unknown_peers_closed() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert!(check_loopback_uid(peer, 80, 1000, table, "").is_err());
    }

    #[test]
    fn document_origins_ignore_userinfo_and_case() {
        assert_eq!(
            document_origin("HTTPS://Example.COM:443/a"),
            Some("https://example.com".into())
        );
        assert_eq!(document_origin("https://user@example.com/"), None);
        assert_eq!(document_origin("file:///etc/passwd"), None);
        assert_eq!(
            document_origin("http://example.com:8080/x"),
            Some("http://example.com:8080".into())
        );
    }

    #[test]
    fn forbidden_schemes_cover_navigation_surface() {
        for url in [
            "file:///etc/passwd",
            "chrome://settings",
            "chrome-extension://abc/main.html",
            "devtools://devtools/bundled/inspector.html",
            "javascript:alert(1)",
            "data:text/html,<h1>hi</h1>",
            "  JAVASCRIPT:alert(1)",
        ] {
            assert!(is_forbidden_scheme(url), "{url}");
        }
        assert!(!is_forbidden_scheme("https://example.com/"));
        assert!(!is_forbidden_scheme("about:blank"));
    }

    #[test]
    fn exfiltration_flags_long_post_and_entropy() {
        let from = Some("https://a.example");
        assert!(exfiltration_check(from, "https://a.example/other", "GET").is_none());
        let long = format!("https://b.example/collect?{}", "x".repeat(129));
        assert!(exfiltration_check(from, &long, "GET").is_some());
        assert!(exfiltration_check(from, "https://b.example/submit", "POST").is_some());
        assert!(exfiltration_check(from, "https://b.example/plain", "GET").is_none());
        // High-entropy token in a short query still signals.
        let token = ")CQ0e!HIpRK0ssukdKsS4)1Plh9RY_xEMWtxa6jxH9zqkm9F";
        assert!(shannon_entropy(token) > 4.5);
        let entropic = format!("https://b.example/cb?token={token}");
        assert!(exfiltration_check(from, &entropic, "GET").is_some());
    }

    #[test]
    fn new_origin_detection_is_origin_based() {
        let known = vec!["https://a.example".to_string()];
        assert!(!is_new_origin("https://a.example/page", &known));
        assert!(is_new_origin("https://b.example/", &known));
        assert!(!is_new_origin("data:text/html,hi", &known));
    }
}
