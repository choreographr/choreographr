//! IP-literal safety classification shared by the workspace's outbound requests.
//!
//! Two outbound request paths fetch a URL the *peer* chose rather than the user:
//! a provider-controlled media URL (the image/video download in
//! `choreo-ai-protocols`), and an OAuth discovery/metadata hop taken from a
//! document a remote MCP server served (`choreo-client-core`'s MCP login). Both
//! must refuse to dial an internal service — loopback, RFC 1918 private, RFC
//! 4193 unique-local, or link-local — or a hostile document could turn the
//! request into a probe of the host's own network.
//!
//! The classifier lives here, in the one leaf crate every caller already
//! depends on, so the two paths share one definition of "blocked range" and
//! cannot drift. It deliberately sees only an *IP literal*: a hostname is left
//! to the caller's own policy, because resolving it here would invite a
//! DNS-rebinding race (the resolver's answer and the later dial's answer can
//! differ), and the paths that use it already accept that residual decision.

use std::net::IpAddr;

/// Whether an IP literal falls in a range an outbound request must never dial
/// from peer-chosen data: loopback, private (RFC 1918 for v4, unique-local
/// `fc00::/7` for v6), or link-local (`169.254/16` for v4, `fe80::/10` for v6),
/// including an IPv4-mapped IPv6 address (which is judged by its embedded v4).
#[must_use]
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                // `is_unique_local` is still unstable on std `Ipv6Addr` —
                // test the RFC 4193 prefix bits directly.
                || (u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]) & 0xfe00) == 0xfc00
                // fe80::/10 link-local: the top 10 bits are 0xfe80..0xfebf.
                || (u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]) & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:10.0.0.1 etc.) — judge the embedded v4.
                || matches!(v6.to_ipv4_mapped(), Some(v4) if v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
    }
}

/// Whether `host` is an IP literal in a blocked range (see [`is_blocked_ip`]).
///
/// The `url` crate renders an IPv6 host in a URL bracketed (`[::1]`), so a
/// leading `[` and trailing `]` are stripped before parsing; a non-IP hostname
/// (e.g. `fal.media`, `mcp.example.com`) is never blocked, per the module's
/// hostname decision.
#[must_use]
pub fn is_blocked_ip_literal(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<IpAddr>().is_ok_and(is_blocked_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_ranges_cover_loopback_private_and_link_local() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.0.1",
        ] {
            assert!(
                is_blocked_ip(blocked.parse().expect("ip")),
                "{blocked} must be classified blocked"
            );
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "203.0.113.9"] {
            assert!(
                !is_blocked_ip(allowed.parse().expect("ip")),
                "{allowed} must be classified allowed"
            );
        }
    }

    #[test]
    fn blocked_ranges_cover_the_ipv6_prefixes() {
        // fc00::/7 (both fc and fd prefixes) and fe80::/10, plus the
        // IPv4-mapped form judged by its embedded v4.
        for blocked in [
            "::1",
            "fc00::1",
            "fd12:3456::a",
            "fe80::1",
            "::ffff:10.0.0.9",
        ] {
            assert!(
                is_blocked_ip(blocked.parse().expect("ip")),
                "{blocked} must be classified blocked"
            );
        }
        assert!(!is_blocked_ip("2606:4700::1111".parse().expect("ip")));
        assert!(!is_blocked_ip("::ffff:8.8.8.8".parse().expect("ip")));
    }

    #[test]
    fn literal_classifier_strips_ipv6_brackets_and_ignores_hostnames() {
        assert!(is_blocked_ip_literal("127.0.0.1"));
        assert!(is_blocked_ip_literal("[::1]"), "a bracketed IPv6 loopback");
        assert!(is_blocked_ip_literal("[fe80::1]"));
        assert!(!is_blocked_ip_literal("fal.media"));
        assert!(!is_blocked_ip_literal("mcp.example.com"));
        assert!(!is_blocked_ip_literal("8.8.8.8"));
        assert!(!is_blocked_ip_literal(""), "an empty host is not an IP");
    }
}
