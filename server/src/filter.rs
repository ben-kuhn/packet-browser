use thiserror::Error;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Error, Debug)]
pub enum UrlError {
    #[error("Blocked protocol: {0}")]
    BlockedProtocol(String),
    #[error("Blocked host: {0}")]
    BlockedHost(String),
    #[error("Invalid URL")]
    InvalidUrl,
}

const BLOCKED_PROTOCOLS: &[&str] = &["file:", "ftp:", "gopher:", "mailto:"];
const BLOCKED_HOSTNAMES: &[&str] = &["localhost"];

pub fn validate_url(url: &str, blocked_ranges: &[String]) -> Result<(), UrlError> {
    let url_lower = url.to_lowercase();

    // Check blocked protocols
    for proto in BLOCKED_PROTOCOLS {
        if url_lower.starts_with(proto) {
            return Err(UrlError::BlockedProtocol(proto.to_string()));
        }
    }

    // Ensure http or https
    if !url_lower.starts_with("http://") && !url_lower.starts_with("https://") {
        return Err(UrlError::InvalidUrl);
    }

    // Extract host from URL
    let host = extract_host(&url_lower)?;

    // Check blocked hostnames
    for blocked in BLOCKED_HOSTNAMES {
        if host == *blocked {
            return Err(UrlError::BlockedHost(host));
        }
    }

    // Check if host is an IP address in blocked ranges
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_ip_in_blocked_ranges(&ip, blocked_ranges) {
            return Err(UrlError::BlockedHost(host));
        }
    }

    Ok(())
}

fn extract_host(url: &str) -> Result<String, UrlError> {
    let without_proto = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or(UrlError::InvalidUrl)?;

    let host_part = without_proto.split('/').next().unwrap_or("");
    let host = host_part.split(':').next().unwrap_or("");

    if host.is_empty() {
        return Err(UrlError::InvalidUrl);
    }

    Ok(host.to_string())
}

fn is_ip_in_blocked_ranges(ip: &IpAddr, blocked_ranges: &[String]) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            for range in blocked_ranges {
                if let Some((network, prefix)) = range.split_once('/') {
                    if let (Ok(net_ip), Ok(prefix_len)) = (network.parse::<Ipv4Addr>(), prefix.parse::<u8>()) {
                        if is_ipv4_in_cidr(ipv4, &net_ip, prefix_len) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        IpAddr::V6(_) => ip.is_loopback(),
    }
}

fn is_ipv4_in_cidr(ip: &Ipv4Addr, network: &Ipv4Addr, prefix_len: u8) -> bool {
    let ip_bits = u32::from_be_bytes(ip.octets());
    let net_bits = u32::from_be_bytes(network.octets());
    let mask = if prefix_len == 0 { 0 } else { !0u32 << (32 - prefix_len) };
    (ip_bits & mask) == (net_bits & mask)
}
