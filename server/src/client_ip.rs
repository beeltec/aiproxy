use std::net::{IpAddr, SocketAddr};

use anyhow::Context;
use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::request::Parts;
use ipnet::IpNet;

/// Reverse proxies that may set `X-Forwarded-For`.
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies(Vec<IpNet>);

impl TrustedProxies {
    pub fn parse(list: &str) -> anyhow::Result<Self> {
        list.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<IpNet>()
                    .or_else(|_| s.parse::<IpAddr>().map(IpNet::from))
                    .with_context(|| format!("AIPROXY_TRUSTED_PROXIES: `{s}` is not an IP address or CIDR"))
            })
            .collect::<anyhow::Result<_>>()
            .map(Self)
    }

    fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.iter().any(|net| net.contains(&ip))
    }

    /// Reads `X-Forwarded-For` from right to left and stops at the first address that is not a trusted proxy.
    pub fn client_ip(&self, peer: IpAddr, forwarded_for: Option<&str>) -> IpAddr {
        if !self.contains(peer) {
            return peer.to_canonical();
        }
        let mut client = peer.to_canonical();
        for hop in forwarded_for.unwrap_or_default().rsplit(',') {
            let Ok(ip) = hop.trim().parse::<IpAddr>() else { break };
            client = ip.to_canonical();
            if !self.contains(ip) {
                break;
            }
        }
        client
    }
}

/// The client IP address of the request.
pub struct ClientIp(pub IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    TrustedProxies: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip())
            .unwrap_or(IpAddr::from([0, 0, 0, 0]));
        // A proxy can add its own header line, so all lines count, in order.
        let forwarded = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        Ok(Self(TrustedProxies::from_ref(state).client_ip(peer, Some(&forwarded))))
    }
}
