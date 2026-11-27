//! SSRF guard for `web_read`: only public addresses are ever connected to.
//! The check is done in the resolver ureq uses, so the vetted address is the
//! one connected to (no second lookup: no DNS rebinding).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

fn public_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast
        || o[0] >= 240) // reserved
}

fn public_v6(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = a.segments();
    if a.is_unspecified() || a.is_loopback() || a.is_multicast() {
        return false;
    }
    if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 {
        return false; // unique local, link-local, site-local
    }
    if s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0) {
        return false; // documentation, Teredo
    }
    if s[0] == 0x0100 && s[1..4].iter().all(|x| *x == 0) {
        return false; // discard-only 100::/64
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 1 {
        return false; // local-use NAT64 64:ff9b:1::/48
    }
    if s[..4].iter().all(|x| *x == 0) && s[4] == 0xffff && s[5] == 0 {
        return false; // SIIT ::ffff:0:0:0/96
    }
    // embedded IPv4: IPv4-compatible (::a.b.c.d), NAT64, 6to4
    let o = a.octets();
    if s[..6].iter().all(|x| *x == 0) {
        return public_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
        return public_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if s[0] == 0x2002 {
        return public_v4(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    true
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

/// Resolver that drops every non-public address. `allow` lists exact
/// `host:port` strings exempt from the check (tests only, see `Web`).
pub struct Vetted {
    pub allow: Vec<String>,
}

impl ureq::Resolver for Vetted {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        // the lookup may hang: bound the wait (the helper thread may outlive us)
        let (tx, rx) = std::sync::mpsc::channel();
        let nl = netloc.to_owned();
        std::thread::spawn(move || {
            let _ = tx.send(nl.to_socket_addrs().map(|i| i.collect::<Vec<_>>()));
        });
        let all = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns lookup timed out"))??;
        if self.allow.iter().any(|a| a == netloc) {
            return Ok(all);
        }
        let ok: Vec<SocketAddr> = all.into_iter().filter(|a| is_public_ip(a.ip())).collect();
        if ok.is_empty() {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("blocked: {netloc} is not a public address"),
            ))
        } else {
            Ok(ok)
        }
    }
}
