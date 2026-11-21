//! Host-owned address pins. A configured policy has no system DNS fallback;
//! HTTP authority, TLS identity and the port still come from the endpoint URL.
use reqwest::{
    ClientBuilder, Url,
    dns::{Name, Resolve, Resolving},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EgressPolicy {
    pub routes: Vec<EgressRoute>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EgressRoute {
    /// Canonical URL host: lower-case ASCII, with brackets for IPv6 literals.
    pub host: String,
    pub addresses: Vec<IpAddr>,
    /// Explicit destination ports, including default ports such as 443.
    pub ports: Vec<u16>,
}

impl EgressPolicy {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.routes.is_empty() || self.routes.len() > 16 {
            return Err("MCP egress requires between 1 and 16 host routes");
        }
        let mut hosts = BTreeSet::new();
        for route in &self.routes {
            if route.host.len() > 253
                || !route
                    .host
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b".-:[]".contains(&c))
                || Url::parse(&format!("https://{}/", route.host))
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .as_deref()
                    != Some(route.host.as_str())
                || !hosts.insert(&route.host)
            {
                return Err("MCP egress requires unique canonical URL hosts");
            }
            let unique: BTreeSet<_> = route.addresses.iter().copied().collect();
            if unique.is_empty()
                || unique.len() > 8
                || unique.len() != route.addresses.len()
                || unique.iter().any(|ip| {
                    let ip = ip.to_canonical();
                    ip.is_unspecified()
                        || ip.is_multicast()
                        || matches!(ip, IpAddr::V4(ip) if ip.is_broadcast())
                })
            {
                return Err("invalid MCP egress route addresses");
            }
            let ports: BTreeSet<_> = route.ports.iter().copied().collect();
            if ports.is_empty() || ports.len() > 8 || ports.len() != route.ports.len() {
                return Err("MCP egress requires unique explicit destination ports");
            }
            if let Ok(literal) = route.host.trim_matches(['[', ']']).parse::<IpAddr>()
                && route.addresses.as_slice() != [literal]
            {
                return Err("MCP egress IP literals must pin themselves");
            }
        }
        Ok(())
    }

    pub(crate) fn require(&self, url: &Url) -> Result<(), &'static str> {
        if self.routes.iter().any(|route| {
            Some(route.host.as_str()) == url.host_str()
                && route
                    .ports
                    .contains(&url.port_or_known_default().unwrap_or_default())
        }) {
            Ok(())
        } else {
            Err("MCP endpoint has no configured egress route")
        }
    }

    pub(crate) fn apply(&self, builder: ClientBuilder) -> ClientBuilder {
        let addresses = self
            .routes
            .iter()
            .map(|route| {
                let addresses: Arc<[SocketAddr]> = route
                    .addresses
                    .iter()
                    .map(|ip| SocketAddr::new(*ip, 0))
                    .collect();
                (route.host.clone(), addresses)
            })
            .collect();
        builder.dns_resolver(Arc::new(PinnedHosts(addresses)))
    }
}

struct PinnedHosts(BTreeMap<String, Arc<[SocketAddr]>>);

impl Resolve for PinnedHosts {
    fn resolve(&self, name: Name) -> Resolving {
        let addresses = self.0.get(name.as_str()).cloned();
        Box::pin(async move {
            let addresses =
                addresses.ok_or_else(|| io::Error::other("MCP egress host is not pinned"))?;
            Ok(Box::new((0..addresses.len()).map(move |i| addresses[i])) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn configured_hosts_addresses_and_ports_must_be_canonical_and_unique() {
        for routes in [
            json!([]),
            json!([{"host":"MCP.example.test","addresses":["192.0.2.1"],"ports":[443]}]),
            json!([{"host":"mcp.example.test","addresses":[],"ports":[443]}]),
            json!([{"host":"mcp.example.test","addresses":["192.0.2.1","192.0.2.1"],"ports":[443]}]),
            json!([{"host":"mcp.example.test","addresses":["0.0.0.0"],"ports":[443]}]),
            json!([{"host":"mcp.example.test","addresses":["192.0.2.1"],"ports":[]}]),
            json!([{"host":"mcp.example.test","addresses":["192.0.2.1"],"ports":[443,443]}]),
            json!([{"host":"[::1]","addresses":["::"],"ports":[443]}]),
        ] {
            let policy: EgressPolicy = serde_json::from_value(json!({"routes":routes})).unwrap();
            assert!(policy.validate().is_err(), "accepted policy: {policy:?}");
        }
    }

    #[test]
    fn endpoint_authority_requires_the_pinned_host_and_port_pair() {
        let policy: EgressPolicy = serde_json::from_value(json!({"routes":[
            {"host":"mcp.example.test","addresses":["192.0.2.10"],"ports":[443]}
        ]}))
        .unwrap();
        assert!(policy.validate().is_ok());
        assert!(
            policy
                .require(&Url::parse("https://mcp.example.test/v1").unwrap())
                .is_ok()
        );
        for target in [
            "https://mcp.example.test:8443/v1",
            "https://other.example.test/v1",
            "http://mcp.example.test/v1",
        ] {
            assert!(
                policy.require(&Url::parse(target).unwrap()).is_err(),
                "allowed {target}"
            );
        }
    }

    #[test]
    fn literal_ip_routes_cannot_redirect_to_another_address() {
        let policy: EgressPolicy = serde_json::from_value(json!({"routes":[
            {"host":"192.0.2.10","addresses":["192.0.2.11"],"ports":[443]}
        ]}))
        .unwrap();
        assert!(policy.validate().is_err());
    }
}
