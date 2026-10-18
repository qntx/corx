//! Public-address classification for the SSRF guard and rate-limit keys.

use core::net::{IpAddr, Ipv6Addr};

/// Denied IPv4 ranges as `(network, prefix_len)` pairs.
const IPV4_DENY: &[(u32, u32)] = &[
    (0x0000_0000, 8),  // 0.0.0.0/8 "this network"
    (0x0a00_0000, 8),  // 10.0.0.0/8 private
    (0x6440_0000, 10), // 100.64.0.0/10 CGNAT
    (0x7f00_0000, 8),  // 127.0.0.0/8 loopback
    (0xa9fe_0000, 16), // 169.254.0.0/16 link-local
    (0xac10_0000, 12), // 172.16.0.0/12 private
    (0xc000_0000, 24), // 192.0.0.0/24 IETF protocol assignments
    (0xc000_0200, 24), // 192.0.2.0/24 TEST-NET-1
    (0xc058_6300, 24), // 192.88.99.0/24 6to4 relay
    (0xc0a8_0000, 16), // 192.168.0.0/16 private
    (0xc612_0000, 15), // 198.18.0.0/15 benchmarking
    (0xc633_6400, 24), // 198.51.100.0/24 TEST-NET-2
    (0xcb00_7100, 24), // 203.0.113.0/24 TEST-NET-3
    (0xe000_0000, 4),  // 224.0.0.0/4 multicast
    (0xf000_0000, 4),  // 240.0.0.0/4 reserved
];

const fn in_prefix(addr: u32, network: u32, len: u32) -> bool {
    addr >> (32 - len) == network >> (32 - len)
}

const fn is_public_v4(addr: u32) -> bool {
    let mut i = 0;
    while i < IPV4_DENY.len() {
        let (net, len) = IPV4_DENY[i];
        if in_prefix(addr, net, len) {
            return false;
        }
        i += 1;
    }
    true
}

const fn is_public_v6(addr: Ipv6Addr) -> bool {
    let a = u128::from_be_bytes(addr.octets());
    // 2000::/3 — the only globally routable unicast block.
    if a >> 125 != 0b001 {
        return false;
    }
    // Exceptions inside 2000::/3 that are not globally routable.
    a >> 105 != 0x10_0080       // 2001::/23 reserved/IETF
        && a >> 96 != 0x2001_0db8 // 2001:db8::/32 documentation
        && a >> 112 != 0x2002   // 2002::/16 6to4
        && a >> 108 != 0x3_fff0 // 3fff::/20 documentation
}

/// Whether `ip` is a globally routable public address.
///
/// IPv4-mapped IPv6 addresses are evaluated by the IPv4 rules.
#[must_use]
pub const fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(u32::from_be_bytes(v4.octets())),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(u32::from_be_bytes(v4.octets())),
            None => is_public_v6(v6),
        },
    }
}

/// Rate-limit key for an address: IPv6 traffic is aggregated per /64,
/// IPv4 (including v4-mapped v6) is keyed by the exact address.
#[must_use]
pub fn rate_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || {
                IpAddr::V6(Ipv6Addr::from(
                    u128::from_be_bytes(v6.octets()) & (u128::MAX << 64),
                ))
            },
            IpAddr::V4,
        ),
    }
}

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(s: &str) -> IpAddr {
        s.parse().expect("valid ipv6")
    }

    #[test]
    fn ipv4_deny_table() {
        let blocked = [
            v4(0, 0, 0, 0),
            v4(0, 255, 255, 255),
            v4(10, 0, 0, 0),
            v4(10, 255, 255, 255),
            v4(100, 64, 0, 0),
            v4(100, 127, 255, 255),
            v4(127, 0, 0, 1),
            v4(127, 255, 255, 255),
            v4(169, 254, 0, 0),
            v4(169, 254, 169, 254),
            v4(172, 16, 0, 0),
            v4(172, 31, 255, 255),
            v4(192, 0, 0, 0),
            v4(192, 0, 0, 255),
            v4(192, 0, 2, 1),
            v4(192, 88, 99, 1),
            v4(192, 168, 0, 0),
            v4(192, 168, 255, 255),
            v4(198, 18, 0, 0),
            v4(198, 19, 255, 255),
            v4(198, 51, 100, 9),
            v4(203, 0, 113, 7),
            v4(224, 0, 0, 1),
            v4(239, 255, 255, 255),
            v4(240, 0, 0, 0),
            v4(255, 255, 255, 255),
        ];
        for ip in blocked {
            assert!(!is_public(ip), "{ip} must be blocked");
        }
    }

    #[test]
    fn ipv4_public_edges() {
        let public = [
            v4(1, 1, 1, 1),
            v4(8, 8, 8, 8),
            v4(11, 0, 0, 1),
            v4(100, 63, 255, 255),
            v4(100, 128, 0, 0),
            v4(172, 15, 255, 255),
            v4(172, 32, 0, 1),
            v4(192, 0, 1, 0),
            v4(192, 0, 3, 0),
            v4(192, 167, 255, 255),
            v4(192, 169, 0, 0),
            v4(198, 17, 255, 255),
            v4(198, 20, 0, 0),
            v4(223, 255, 255, 255),
        ];
        for ip in public {
            assert!(is_public(ip), "{ip} must be public");
        }
    }

    #[test]
    fn ipv6_classification() {
        let blocked = [
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "2001::1",      // inside 2001::/23
            "2001:1ff::1",  // top of 2001::/23
            "2001:db8::1",  // documentation
            "2002::1",      // 6to4
            "3fff::1",      // documentation
            "3fff:0fff::1", // top of 3fff::/20
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
        ];
        for s in blocked {
            assert!(!is_public(v6(s)), "{s} must be blocked");
        }
        let public = [
            "2001:200::1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "3fff:1000::1",
        ];
        for s in public {
            assert!(is_public(v6(s)), "{s} must be public");
        }
    }

    #[test]
    fn rate_key_masks_ipv6() {
        assert_eq!(
            rate_key("2606:4700:4700::1234".parse().expect("ip")),
            "2606:4700:4700::".parse::<IpAddr>().expect("ip")
        );
        assert_eq!(
            rate_key(v4(203, 0, 114, 9)),
            v4(203, 0, 114, 9),
            "IPv4 is keyed unmasked"
        );
        assert_eq!(
            rate_key("::ffff:10.1.2.3".parse().expect("ip")),
            v4(10, 1, 2, 3),
            "v4-mapped collapses to the IPv4 key"
        );
    }
}
