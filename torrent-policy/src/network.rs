// SPDX-License-Identifier: Apache-2.0
//! Peer transport destination policy. Native policy is never deserialized from input.
use std::net::{IpAddr, SocketAddr};

pub const PUBLIC_BOOTSTRAP_VERSION: &str = "public_dht_tcp_v1-bootstrap-1";
pub const PUBLIC_BOOTSTRAP: &[&str] = &["dht.transmissionbt.com:6881", "dht.libtorrent.org:25401"];

#[derive(Clone, Default)]
pub enum NetworkPolicy {
    #[default]
    Gateway,
    PublicDhtTcpV1,
    #[cfg(any(test, feature = "test-network-policy"))]
    OwnedPrivateTcp(Vec<SocketAddr>),
}
impl std::fmt::Debug for NetworkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Gateway => "Gateway",
            Self::PublicDhtTcpV1 => "PublicDhtTcpV1",
            #[cfg(any(test, feature = "test-network-policy"))]
            Self::OwnedPrivateTcp(_) => "OwnedPrivateTcp(<redacted>)",
        })
    }
}
impl NetworkPolicy {
    pub fn is_native(&self) -> bool {
        !matches!(self, Self::Gateway)
    }
    pub fn allows(&self, addr: SocketAddr) -> bool {
        if addr.port() == 0 {
            return false;
        }
        match self {
            Self::Gateway => true,
            Self::PublicDhtTcpV1 => globally_routable(addr.ip()),
            #[cfg(any(test, feature = "test-network-policy"))]
            Self::OwnedPrivateTcp(owned) => {
                owned.contains(&addr)
                    && match addr.ip() {
                        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
                        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
                    }
            }
        }
    }
    pub fn validate_destination(&self, addr: SocketAddr) -> anyhow::Result<()> {
        anyhow::ensure!(self.allows(addr), "Network destination denied");
        Ok(())
    }
    /// Reject the entire answer set before any connection, including mixed answers.
    pub fn validate_resolution(&self, addrs: &[SocketAddr]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !addrs.is_empty() && addrs.iter().all(|a| self.allows(*a)),
            "Network resolution denied"
        );
        Ok(())
    }
    pub fn allows_dht(&self) -> bool {
        match self {
            Self::Gateway | Self::PublicDhtTcpV1 => true,
            #[cfg(any(test, feature = "test-network-policy"))]
            Self::OwnedPrivateTcp(_) => false,
        }
    }
}
pub fn globally_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && ((b == 0 && (c == 0 || c == 2))
                        || (b == 88 && c == 99)
                        || (b == 31 && c == 196)
                        || (b == 52 && c == 193)
                        || (b == 175 && c == 48)
                        || b == 168))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Admit only global unicast and reject special-use and transition prefixes.
            (0x2000..=0x3fff).contains(&s[0])
                && !(s[0] == 0x2001 && (s[1] <= 0x1ff || s[1] == 0xdb8))
                && s[0] != 0x2002
                && s[0] != 0x3fff
                && !(s[0] == 0x2620 && s[1] == 0x4f && s[2] == 0x8000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_policy_denies_special_and_transition_destinations() {
        for ip in [
            "0.0.0.0",
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.1.1",
            "172.31.255.255",
            "192.0.0.9",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.175.48.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.1.1",
            "198.19.1.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.1.2.3",
            "240.1.2.3",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "100::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "2002:0808:0808::1",
            "3fff::1",
            "2620:4f:8000::1",
        ] {
            assert!(
                !NetworkPolicy::PublicDhtTcpV1.allows(SocketAddr::new(ip.parse().unwrap(), 6881)),
                "destination should be denied"
            );
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
        ] {
            assert!(
                NetworkPolicy::PublicDhtTcpV1.allows(SocketAddr::new(ip.parse().unwrap(), 6881))
            );
        }
    }
    #[test]
    fn reject_entire_injected_mixed_dns_set() {
        let policy = NetworkPolicy::PublicDhtTcpV1;
        let good: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let bad: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        assert!(policy.validate_resolution(&[good]).is_ok());
        assert!(policy.validate_resolution(&[good, bad]).is_err());
        assert!(policy.validate_resolution(&[bad, good]).is_err());
        assert!(policy.validate_resolution(&[]).is_err());
        assert!(policy
            .validate_destination("8.8.8.8:0".parse().unwrap())
            .is_err());
    }
    #[test]
    fn isolated_owned_tcp_policy_is_exact_and_has_no_dht() {
        let owned: SocketAddr = "127.0.0.1:49000".parse().unwrap();
        let policy = NetworkPolicy::OwnedPrivateTcp(vec![owned]);
        assert!(policy.allows(owned));
        assert!(!policy.allows("127.0.0.1:49001".parse().unwrap()));
        assert!(!policy.allows("8.8.8.8:49000".parse().unwrap()));
        assert!(!policy.allows_dht());
    }
}
