//! IP-literal safety classification shared by the workspace's outbound requests.
//!
//! Two outbound request paths fetch a URL the *peer* chose rather than the user:
//! a provider-controlled media URL (the image/video download in
//! `choreo-ai-protocols`), and an OAuth discovery/metadata hop taken from a
//! document a remote MCP server served (`choreo-client-core`'s MCP login). Both
//! must refuse to dial an internal service — the unspecified address,
//! `0.0.0.0/8`, loopback, RFC 1918 private, RFC 6598 shared (carrier-grade NAT),
//! RFC 2544 benchmarking, RFC 4193 unique-local, or link-local — or a hostile
//! document could turn the request into a probe of the host's own network.
//!
//! The classifier lives here, in the one leaf crate every caller already
//! depends on, so the two paths share one definition of "blocked range" and
//! cannot drift. It deliberately sees only an *IP literal*: a hostname is left
//! to the caller's own policy, because resolving it here would invite a
//! DNS-rebinding race (the resolver's answer and the later dial's answer can
//! differ), and the paths that use it already accept that residual decision.
//!
//! Over-refusing is the safe direction: a false positive blocks only a bizarre
//! endpoint, while a false negative — an address class the guard does not know —
//! turns the client into an internal-network prober. So the internal-purpose
//! ranges are refused wholesale rather than narrowed to the handful of
//! addresses that are plausibly reachable.

use std::net::{IpAddr, Ipv4Addr};

/// Whether an IPv4 literal is in a range an outbound request must never dial
/// from peer-chosen data.
///
/// The `0.0.0.0/8` "this network" block subsumes the unspecified address
/// (`is_unspecified`): on Linux a connect to `0.0.0.0:port` reaches the local
/// host, so refusing only `0.0.0.0` itself would leave that dial working, and no
/// legitimate remote endpoint lives anywhere in the block. Likewise `100.64.0.0/10`
/// (RFC 6598 carrier-grade NAT shared address space, routinely an internal
/// network) and `198.18.0.0/15` (RFC 2544 benchmarking) are internal-purpose
/// ranges that never host an OAuth endpoint or a provider media file.
///
/// Kept as one helper so the `V4` arm and the IPv4-mapped IPv6 case judge an
/// embedded v4 identically and cannot drift.
fn blocked_v4(v4: Ipv4Addr) -> bool {
    // The first two octets decide every range test below; unpack once.
    let [a, b, _, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        // 0.0.0.0/8 "this network", including the unspecified address.
        || a == 0
        // 100.64.0.0/10 shared address space (carrier-grade NAT).
        || (a == 100 && (64..=127).contains(&b))
        // 198.18.0.0/15 benchmarking.
        || (a == 198 && (18..=19).contains(&b))
}

/// Whether an IP literal falls in a range an outbound request must never dial
/// from peer-chosen data: the unspecified address, `0.0.0.0/8` "this network",
/// loopback, private (RFC 1918 for v4, unique-local `fc00::/7` for v6), shared
/// carrier-grade NAT (`100.64.0.0/10`), benchmarking (`198.18.0.0/15`), or
/// link-local (`169.254/16` for v4, `fe80::/10` for v6), including an
/// IPv4-mapped IPv6 address (which is judged by its embedded v4).
///
/// The ranges are the ones a peer-chosen URL could use to reach the host's own
/// machine or its LAN: an SSRF guard has to refuse them all, so a hostile
/// document cannot aim a fetch at a local service.
#[must_use]
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                // `::` — the v6 unspecified address, which a dual-stack dial
                // can send to the local host just as `0.0.0.0` can on v4.
                || v6.is_unspecified()
                // `is_unique_local` is still unstable on std `Ipv6Addr` —
                // test the RFC 4193 prefix bits directly.
                || (u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]) & 0xfe00) == 0xfc00
                // fe80::/10 link-local: the top 10 bits are 0xfe80..0xfebf.
                || (u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]) & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:10.0.0.1 etc.) — judge the embedded v4
                // through the same predicate as the `V4` arm, so the two
                // coverage sets are identical by construction.
                || v6.to_ipv4_mapped().is_some_and(blocked_v4)
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
        for allowed in [
            // Just outside the shared/CGNAT and benchmarking ranges, so a
            // boundary regression in either predicate is caught.
            "100.63.255.255",
            "100.128.0.0",
            "198.17.255.255",
            "198.20.0.0",
            // Ordinary public addresses.
            "8.8.8.8",
            "1.1.1.1",
            "203.0.113.9",
        ] {
            assert!(
                !is_blocked_ip(allowed.parse().expect("ip")),
                "{allowed} must be classified allowed"
            );
        }
    }

    #[test]
    fn blocked_ranges_cover_unspecified_and_shared_address_space() {
        // The unspecified addresses and the internal-purpose ranges a
        // peer-chosen URL must never dial, including the v4-mapped forms so
        // the shared predicate cannot drift from the plain `V4` arm.
        for blocked in [
            "0.0.0.0",
            "0.1.2.3",
            "::",
            "100.64.0.1",
            "100.127.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "::ffff:0.0.0.0",
            "::ffff:100.64.0.1",
        ] {
            assert!(
                is_blocked_ip(blocked.parse().expect("ip")),
                "{blocked} must be classified blocked"
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
        // The unspecified v4 address reaches the local host, so the literal
        // form must refuse it too (not just the parsed `IpAddr` form).
        assert!(is_blocked_ip_literal("0.0.0.0"));
        assert!(!is_blocked_ip_literal("fal.media"));
        assert!(!is_blocked_ip_literal("mcp.example.com"));
        assert!(!is_blocked_ip_literal("8.8.8.8"));
        assert!(!is_blocked_ip_literal(""), "an empty host is not an IP");
    }
}
