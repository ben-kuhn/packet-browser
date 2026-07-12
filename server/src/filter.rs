use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use thiserror::Error;
use url::{Host, Url};

use crate::blocklist::is_domain_blocked;

#[derive(Error, Debug)]
pub enum UrlError {
    #[error("Blocked protocol: {0}")]
    BlockedProtocol(String),
    #[error("Blocked host: {0}")]
    BlockedHost(String),
    #[error("Host could not be resolved: {0}")]
    UnresolvableHost(String),
    #[error("Invalid URL")]
    InvalidUrl,
}

const BLOCKED_HOSTNAMES: &[&str] = &[
    "localhost",
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
    "broadcasthost",
];

/// Validate a hostname + port and return a **single pinned IP** to connect to.
/// The proxy uses this: it resolves DNS exactly once (here), checks the IP
/// against blocked ranges, and then opens the outbound connection to that
/// same IP — closing the classic DNS-rebinding TOCTOU where the filter and
/// the fetch could each resolve to different addresses.
pub fn resolve_and_pin(
    host: &str,
    port: u16,
    blocked_ranges: &[String],
) -> Result<IpAddr, UrlError> {
    let host_lc = host.to_ascii_lowercase();
    if BLOCKED_HOSTNAMES.iter().any(|h| *h == host_lc) {
        return Err(UrlError::BlockedHost(host_lc));
    }
    // Consult the runtime domain blocklist (populated from BLOCKLIST_URLS).
    // Cheap in-memory check, no DNS lookup needed.
    if is_domain_blocked(&host_lc) {
        return Err(UrlError::BlockedHost(host_lc));
    }

    // If the caller handed us an IP literal, just range-check it.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip_is_blocked(&ip, blocked_ranges) {
            return Err(UrlError::BlockedHost(ip.to_string()));
        }
        return Ok(ip);
    }
    // Ditto for bracketed IPv6.
    if let Some(inner) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if let Ok(ip) = inner.parse::<IpAddr>() {
            if ip_is_blocked(&ip, blocked_ranges) {
                return Err(UrlError::BlockedHost(ip.to_string()));
            }
            return Ok(ip);
        }
    }

    // Hostname: resolve once, pick the first IP that's not in a blocked range.
    // If the very first answer is blocked, treat the whole name as blocked --
    // safer than iterating and picking a later "clean" answer, which would let
    // an attacker hide a blocked target behind a mix of clean names.
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| UrlError::UnresolvableHost(host.to_string()))?;
    let first = addrs
        .next()
        .ok_or_else(|| UrlError::UnresolvableHost(host.to_string()))?
        .ip();
    if ip_is_blocked(&first, blocked_ranges) {
        return Err(UrlError::BlockedHost(format!(
            "{} (resolves to {})",
            host, first
        )));
    }
    Ok(first)
}

pub fn validate_url(url: &str, blocked_ranges: &[String]) -> Result<(), UrlError> {
    let parsed = Url::parse(url).map_err(|_| UrlError::InvalidUrl)?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(UrlError::BlockedProtocol(other.to_string())),
    }

    let host = parsed.host().ok_or(UrlError::InvalidUrl)?;

    match host {
        Host::Domain(name) => {
            let name_lc = name.to_ascii_lowercase();
            if BLOCKED_HOSTNAMES.iter().any(|h| *h == name_lc) {
                return Err(UrlError::BlockedHost(name_lc));
            }
            if is_domain_blocked(&name_lc) {
                return Err(UrlError::BlockedHost(name_lc));
            }

            // Resolve and check every returned IP against the blocked ranges.
            // For the top-level navigation URL this is a coarse check; the
            // in-process proxy does the same check with a pinned IP at every
            // subresource fetch, so any rebinding window is closed there.
            let port = parsed.port_or_known_default().unwrap_or(80);
            let resolved = (name, port)
                .to_socket_addrs()
                .map_err(|_| UrlError::UnresolvableHost(name.to_string()))?;
            for addr in resolved {
                if ip_is_blocked(&addr.ip(), blocked_ranges) {
                    return Err(UrlError::BlockedHost(format!(
                        "{} (resolves to {})",
                        name,
                        addr.ip()
                    )));
                }
            }
        }
        Host::Ipv4(ip) => {
            if ip_is_blocked(&IpAddr::V4(ip), blocked_ranges) {
                return Err(UrlError::BlockedHost(ip.to_string()));
            }
        }
        Host::Ipv6(ip) => {
            if ip_is_blocked(&IpAddr::V6(ip), blocked_ranges) {
                return Err(UrlError::BlockedHost(ip.to_string()));
            }
        }
    }

    Ok(())
}

fn ip_is_blocked(ip: &IpAddr, blocked_ranges: &[String]) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_blocked(v4, blocked_ranges),
        IpAddr::V6(v6) => ipv6_is_blocked(v6, blocked_ranges),
    }
}

fn ipv4_is_blocked(ip: &Ipv4Addr, blocked_ranges: &[String]) -> bool {
    for range in blocked_ranges {
        if let Some((network, prefix)) = range.split_once('/') {
            if let (Ok(net_ip), Ok(prefix_len)) =
                (network.parse::<Ipv4Addr>(), prefix.parse::<u8>())
            {
                if prefix_len <= 32 && is_ipv4_in_cidr(ip, &net_ip, prefix_len) {
                    return true;
                }
            }
        }
    }
    false
}

// Block all the IPv6 ranges that could route to the host's own networks. The
// IPv4 blocked_ranges list does not cover these because the operator's threat
// model is expressed in IPv4 terms.
fn ipv6_is_blocked(ip: &Ipv6Addr, blocked_ranges: &[String]) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }

    // Unique-local fc00::/7
    let octets = ip.octets();
    if octets[0] & 0xfe == 0xfc {
        return true;
    }
    // Link-local fe80::/10
    if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
        return true;
    }
    // Discard prefix 100::/64 (RFC 6666)
    if octets[..8] == [0x01, 0x00, 0, 0, 0, 0, 0, 0] {
        return true;
    }
    // IPv4-mapped ::ffff:0:0/96 - re-check the embedded v4 against the v4 rules
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_blocked(&v4, blocked_ranges);
    }
    // 6to4 2002::/16 - the next 32 bits are the embedded IPv4 address
    if octets[0] == 0x20 && octets[1] == 0x02 {
        let v4 = Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]);
        return ipv4_is_blocked(&v4, blocked_ranges);
    }

    false
}

fn is_ipv4_in_cidr(ip: &Ipv4Addr, network: &Ipv4Addr, prefix_len: u8) -> bool {
    let ip_bits = u32::from_be_bytes(ip.octets());
    let net_bits = u32::from_be_bytes(network.octets());
    let mask = if prefix_len == 0 {
        0
    } else {
        !0u32 << (32 - prefix_len)
    };
    (ip_bits & mask) == (net_bits & mask)
}
