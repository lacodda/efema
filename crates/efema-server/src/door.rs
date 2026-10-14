//! The door: which requests the relay answers at all.
//!
//! The relay has no TLS of its own. It answers two kinds of request: one made
//! on its own machine, and one that came in over HTTPS through the reverse
//! proxy in front of it. Anything else is plain HTTP that crossed a network -
//! an access key in the clear - and is refused before the key is looked at.
//!
//! "Its own machine" and "the proxy" are the same test: the connection comes
//! from an address the relay trusts - loopback, unless the operator names
//! more. A trusted peer that says nothing about forwarding made the request
//! itself; one that forwards says how the request reached it, and only
//! `https` passes. A proxy that forwards without saying is refused: there is
//! no telling what it took the request over.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use axum::http::HeaderMap;

/// Addresses the relay trusts to be itself or the proxy in front of it: an
/// address and a prefix length, `127.0.0.0/8` or `fd00::/8`; a bare address
/// is the network of that address alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    address: IpAddr,
    prefix: u8,
}

/// Why a text is not a network.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not an address or a network: write it as 10.0.0.2, 10.0.0.0/8, ::1 or fd00::/8")]
pub struct InvalidNetwork(String);

impl Network {
    /// The loopback networks of both families: the relay's own machine.
    pub const LOOPBACK: [Network; 2] = [
        Network { address: IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 0)), prefix: 8 },
        Network { address: IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), prefix: 128 },
    ];

    /// Whether `address` is in this network. An IPv4 address seen through an
    /// IPv6 socket (`::ffff:127.0.0.1`) is taken as the IPv4 address it is.
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.address, address.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                same_prefix(&network.octets(), &address.octets(), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                same_prefix(&network.octets(), &address.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

fn same_prefix(network: &[u8], address: &[u8], prefix: u8) -> bool {
    let whole = usize::from(prefix / 8);
    let rest = prefix % 8;
    if network[..whole] != address[..whole] {
        return false;
    }
    rest == 0 || {
        let mask = 0xffu8 << (8 - rest);
        network[whole] & mask == address[whole] & mask
    }
}

impl FromStr for Network {
    type Err = InvalidNetwork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidNetwork(s.to_string());
        let (address, prefix) = match s.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (s, None),
        };
        let address: IpAddr = address.parse().map_err(|_| invalid())?;
        let bits = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            // Digits only: `u8::from_str` takes a leading `+`.
            Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse::<u8>().ok().filter(|p| *p <= bits).ok_or_else(invalid)?
            }
            Some(_) => return Err(invalid()),
            None => bits,
        };
        Ok(Self { address, prefix })
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

/// How a request the relay answers reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Made on the relay's own machine - or by a trusted peer that did not
    /// forward it.
    Local,
    /// Through a trusted proxy, which took it over HTTPS.
    Door,
}

/// The headers by which a proxy says it forwarded a request.
const FORWARDING: [&str; 4] = ["forwarded", "x-forwarded-proto", "x-forwarded-for", "x-forwarded-host"];

/// How the request from `peer` with `headers` reached the relay, or why it is
/// refused.
pub(crate) fn origin(peer: IpAddr, headers: &HeaderMap, trusted: &[Network]) -> Result<Origin, String> {
    let peer = peer.to_canonical();
    if !trusted.iter().any(|network| network.contains(peer)) {
        return Err(format!(
            "plain HTTP from {peer}: the relay answers only through a reverse proxy that takes requests over HTTPS"
        ));
    }
    if !FORWARDING.iter().any(|name| headers.contains_key(*name)) {
        return Ok(Origin::Local);
    }
    let protocols = protocols(headers);
    if protocols.is_empty() {
        return Err(
            "the proxy forwarded the request without saying how it reached it: have it set X-Forwarded-Proto".into()
        );
    }
    match protocols.iter().find(|protocol| !protocol.eq_ignore_ascii_case("https")) {
        Some(protocol) => Err(format!(
            "the request reached the proxy over {protocol}: the relay answers only what came in over HTTPS"
        )),
        None => Ok(Origin::Door),
    }
}

/// Every protocol the forwarding headers name: each value of
/// `X-Forwarded-Proto`, and each `proto=` of `Forwarded` (RFC 7239), for
/// every hop. All of them have to be `https`.
fn protocols(headers: &HeaderMap) -> Vec<String> {
    let mut found = Vec::new();
    for value in headers.get_all("x-forwarded-proto") {
        let text = value.to_str().unwrap_or("unreadable");
        found.extend(text.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    }
    for value in headers.get_all("forwarded") {
        let text = value.to_str().unwrap_or("proto=unreadable");
        for pair in text.split([',', ';']) {
            if let Some((name, protocol)) = pair.split_once('=')
                && name.trim().eq_ignore_ascii_case("proto")
            {
                found.push(protocol.trim().trim_matches('"').to_string());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn net(text: &str) -> Network {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn a_network_holds_its_addresses_and_no_others() {
        assert!(net("10.0.0.0/8").contains(ip("10.200.3.4")));
        assert!(!net("10.0.0.0/8").contains(ip("11.0.0.1")));
        assert!(net("192.168.1.0/25").contains(ip("192.168.1.127")));
        assert!(!net("192.168.1.0/25").contains(ip("192.168.1.128")));
        assert!(net("10.0.0.2").contains(ip("10.0.0.2")));
        assert!(!net("10.0.0.2").contains(ip("10.0.0.3")));
        assert!(net("0.0.0.0/0").contains(ip("8.8.8.8")));
        assert!(net("fd00::/8").contains(ip("fd12::1")));
        assert!(!net("fd00::/8").contains(ip("fe80::1")));
        assert!(!net("10.0.0.0/8").contains(ip("::1")), "families do not mix");
        assert!(Network::LOOPBACK[0].contains(ip("::ffff:127.0.0.1")), "a mapped address is its IPv4 self");
    }

    #[test]
    fn a_network_is_written_one_way() {
        for bad in ["10.0.0.0/33", "::/129", "10.0.0.0/", "10.0.0.0/+8", "localhost", "10.0.0/8", ""] {
            assert!(bad.parse::<Network>().is_err(), "{bad}");
        }
        assert_eq!(net("10.0.0.2").to_string(), "10.0.0.2/32");
    }

    #[test]
    fn the_door_lets_in_this_machine_and_https_through_the_proxy() {
        let loopback = &Network::LOOPBACK;
        assert_eq!(origin(ip("127.0.0.1"), &headers(&[]), loopback), Ok(Origin::Local));
        assert_eq!(origin(ip("::1"), &headers(&[]), loopback), Ok(Origin::Local));
        let https = headers(&[("x-forwarded-proto", "https"), ("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(origin(ip("127.0.0.1"), &https, loopback), Ok(Origin::Door));
        let rfc = headers(&[("forwarded", "for=203.0.113.9;proto=https;host=relay.example.com")]);
        assert_eq!(origin(ip("127.0.0.1"), &rfc, loopback), Ok(Origin::Door));
        let trusted = [net("172.18.0.0/16")];
        assert_eq!(origin(ip("172.18.0.5"), &https, &trusted), Ok(Origin::Door));
    }

    #[test]
    fn the_door_refuses_plain_http_from_outside() {
        let loopback = &Network::LOOPBACK;
        // Straight from another machine: the relay on 0.0.0.0 by mistake.
        assert!(origin(ip("192.168.1.20"), &headers(&[]), loopback).is_err());
        // Even saying https: only a trusted proxy is believed.
        let https = headers(&[("x-forwarded-proto", "https")]);
        assert!(origin(ip("192.168.1.20"), &https, loopback).is_err());
        // A proxy that took it over plain HTTP.
        let http = headers(&[("x-forwarded-proto", "http"), ("x-forwarded-for", "203.0.113.9")]);
        assert!(origin(ip("127.0.0.1"), &http, loopback).unwrap_err().contains("over http"));
        let rfc = headers(&[("forwarded", "for=203.0.113.9;proto=http")]);
        assert!(origin(ip("127.0.0.1"), &rfc, loopback).is_err());
        // A chain of proxies where one hop was plain.
        let chain = headers(&[("x-forwarded-proto", "https, http")]);
        assert!(origin(ip("127.0.0.1"), &chain, loopback).is_err());
        let mixed = headers(&[("x-forwarded-proto", "https"), ("forwarded", "proto=http")]);
        assert!(origin(ip("127.0.0.1"), &mixed, loopback).is_err());
        // A proxy that forwards without saying how the request came.
        let silent = headers(&[("x-forwarded-for", "203.0.113.9")]);
        assert!(origin(ip("127.0.0.1"), &silent, loopback).unwrap_err().contains("X-Forwarded-Proto"));
        // Nothing trusted: not even this machine.
        assert!(origin(ip("127.0.0.1"), &headers(&[]), &[]).is_err());
    }
}
