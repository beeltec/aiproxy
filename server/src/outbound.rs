//! Outbound network policy for the base URLs of connections: HTTPS only, and no private,
//! loopback, link-local or metadata addresses. `AIPROXY_ALLOW_PRIVATE_UPSTREAMS` lifts both.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// True when the address must not be reached from the gateway.
pub fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => blocked_v4(ip),
        IpAddr::V6(ip) => {
            // IPv4-mapped (::ffff:a.b.c.d) and NAT64 (64:ff9b::a.b.c.d) addresses carry an IPv4 target.
            if let Some(v4) = ip.to_ipv4_mapped() {
                return blocked_v4(v4);
            }
            let segments = ip.segments();
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = segments[6].to_be_bytes();
                let [c, d] = segments[7].to_be_bytes();
                return blocked_v4(Ipv4Addr::new(a, b, c, d));
            }
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                // Documentation range 2001:db8::/32.
                || segments[..2] == [0x2001, 0xdb8]
        }
    }
}

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        // Shared address space 100.64.0.0/10 (carrier-grade NAT, some cloud metadata).
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments 192.0.0.0/24 and benchmarking 198.18.0.0/15.
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b))
        // Reserved 240.0.0.0/4.
        || a >= 240
}

/// Checks a base URL: scheme, and a literal IP host. Host names are checked when they are
/// resolved (see `GuardedResolver`).
pub fn check_url(url: &Url, allow_private: bool) -> Result<(), String> {
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err("The base URL must use https.".into()),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The base URL must not contain a user name or password.".into());
    }
    let ip = match url.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(Host::Domain(_)) => return Ok(()),
        None => return Err("The base URL has no host.".into()),
    };
    if !allow_private && blocked(ip) {
        return Err("The base URL points to a private or reserved address.".into());
    }
    Ok(())
}

/// Resolves host names and refuses the connection when any address is blocked.
struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if let Some(addr) = addrs.iter().find(|addr| blocked(addr.ip())) {
                return Err(format!("{host} resolves to the blocked address {}", addr.ip()).into());
            }
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

/// The HTTP client for connection upstreams. It follows no redirects and, unless private
/// upstreams are allowed, checks every resolved address.
pub fn client(allow_private: bool) -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30));
    let builder = if allow_private {
        builder
    } else {
        builder.dns_resolver(Arc::new(GuardedResolver))
    };
    builder.build()
}
