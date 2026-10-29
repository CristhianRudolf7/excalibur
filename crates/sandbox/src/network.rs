//! Host-owned TCP egress policy. Unix runtime is a platform adapter; public
//! configuration and resolver contracts stay independent of operating-system APIs.
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::Arc,
};
#[cfg(unix)]
use std::{
    net::TcpStream,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_secs(60);
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkPolicy {
    Domains(BTreeSet<String>),
    Full,
}
impl NetworkPolicy {
    pub fn contains(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Full, _) => true,
            (Self::Domains(a), Self::Domains(b)) => b.is_subset(a),
            _ => false,
        }
    }
}
/// The host may supply a bounded/native resolver or a deterministic backend
/// fixture. Addresses are subsequently validated and pinned by the broker.
pub trait Resolver: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}
#[derive(Debug)]
pub struct SystemResolver;
impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        (host, port)
            .to_socket_addrs()
            .map(|v| v.collect())
            .map_err(|_| "DNS resolution failed".into())
    }
}
#[derive(Clone)]
pub struct ProxyConfig {
    pub policy: NetworkPolicy,
    /// HTTP defaults; trusted hosts explicitly add non-web service ports.
    pub allowed_ports: BTreeSet<u16>,
    /// Exact private addresses for deliberately authorized LAN/local services.
    /// Empty by default, including under Full: link-local/loopback rebinding is
    /// never inferred from an Internet hostname authorization.
    pub permitted_private: BTreeSet<IpAddr>,
    pub resolver: Arc<dyn Resolver>,
    /// Dynamic per-connection authorization; cloned guards may read durable state.
    pub authorizer: Option<Arc<dyn excalibur_types::runtime::NetworkAuthorizer>>,
}
impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field(
                "domain_count",
                &match &self.policy {
                    NetworkPolicy::Domains(domains) => Some(domains.len()),
                    NetworkPolicy::Full => None,
                },
            )
            .field("port_count", &self.allowed_ports.len())
            .field("private_exception_count", &self.permitted_private.len())
            .field("live_authorizer", &self.authorizer.is_some())
            .finish()
    }
}
impl ProxyConfig {
    pub fn new(policy: NetworkPolicy) -> Self {
        Self {
            policy,
            allowed_ports: [80, 443].into(),
            permitted_private: BTreeSet::new(),
            resolver: Arc::new(SystemResolver),
            authorizer: None,
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.allowed_ports.is_empty() || self.allowed_ports.contains(&0) {
            return Err("proxy requires nonzero explicitly permitted ports".into());
        }
        if let NetworkPolicy::Domains(domains) = &self.policy {
            for domain in domains {
                if canonical_domain(domain)? != *domain {
                    return Err("network domains must be normalized exact DNS names".into());
                }
            }
        }
        Ok(())
    }
    pub fn restricted_by(&self, base: &Self) -> Result<Self, String> {
        self.validate()?;
        base.validate()?;
        if !base.policy.contains(&self.policy)
            || !self.allowed_ports.is_subset(&base.allowed_ports)
            || !self.permitted_private.is_subset(&base.permitted_private)
        {
            return Err("network capability exceeds host boundary".into());
        }
        let mut next = self.clone();
        next.resolver = base.resolver.clone();
        next.authorizer = match (&base.authorizer, &self.authorizer) {
            (Some(a), Some(b)) if !Arc::ptr_eq(a, b) => {
                Some(Arc::new(CombinedAuthorizer(a.clone(), b.clone())))
            }
            (Some(a), _) => Some(a.clone()),
            (_, Some(b)) => Some(b.clone()),
            _ => None,
        };
        Ok(next)
    }
    #[cfg(unix)]
    fn connect(&self, host: &str, port: u16, closed: &AtomicBool) -> Result<TcpStream, String> {
        let host = normalize_host(host)?;
        let deny = |reason: &str| -> Result<TcpStream, String> {
            if let Some(authorizer) = &self.authorizer {
                let _ = authorizer.rejected(&host, port, reason);
            }
            Err(reason.into())
        };
        if !self.allowed_ports.contains(&port) {
            return deny("destination port denied");
        }
        if let NetworkPolicy::Domains(domains) = &self.policy
            && (!domains.contains(&host) || host.parse::<IpAddr>().is_ok())
        {
            return deny("destination domain denied");
        }
        if closed.load(Ordering::Acquire) {
            return Err("proxy closed".into());
        }
        let admission = match self
            .authorizer
            .as_ref()
            .map(|authorizer| authorizer.authorize_connection(&host, port))
            .transpose()
        {
            Ok(admission) => admission,
            Err(_) => return deny("live network authorization denied"),
        };
        let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, port)]
        } else {
            match self.resolver.resolve(&host, port) {
                Ok(addresses) => addresses,
                Err(_) => return deny("destination DNS resolution failed"),
            }
        };
        if addresses.is_empty() || addresses.len() > 64 {
            return deny("DNS returned no bounded address set");
        }
        let addresses = addresses
            .into_iter()
            .map(|address| SocketAddr::new(address.ip(), port))
            .collect::<Vec<_>>();
        // Reject the complete answer when even one address escapes policy. Never
        // connect by hostname after this check (no validation/connect DNS race).
        if addresses.iter().any(|address| {
            !public_ip(address.ip()) && !self.permitted_private.contains(&address.ip())
        }) {
            return deny("DNS resolved to a prohibited private or special-use address");
        }
        for address in addresses {
            if closed.load(Ordering::Acquire) {
                return Err("proxy closed".into());
            }
            if let Some(admission) = &admission
                && admission.revalidate().is_err()
            {
                return deny("network authorization changed during resolution");
            }
            if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
                stream
                    .set_read_timeout(Some(IO_TIMEOUT))
                    .map_err(|_| "upstream timeout configuration failed")?;
                stream
                    .set_write_timeout(Some(IO_TIMEOUT))
                    .map_err(|_| "upstream timeout configuration failed")?;
                return Ok(stream);
            }
        }
        deny("destination connection failed")
    }
}
pub fn canonical_domain(value: &str) -> Result<String, String> {
    let host = normalize_host(value)?;
    if host.parse::<IpAddr>().is_ok() {
        return Err("domain policy requires DNS names, not IP literals".into());
    }
    Ok(host)
}
fn normalize_host(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 253
        || value
            .bytes()
            .any(|b| b <= 32 || matches!(b, b'/' | b'\\' | b'@' | b'*' | b'%' | b'#' | b'?'))
    {
        return Err("invalid destination hostname".into());
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }
    let value = value.trim_end_matches('.');
    let host = url::Host::parse(value).map_err(|_| "invalid destination hostname")?;
    match host {
        url::Host::Domain(domain)
            if !domain.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }) =>
        {
            Ok(domain.to_ascii_lowercase())
        }
        url::Host::Ipv4(ip) => Ok(ip.to_string()),
        url::Host::Ipv6(ip) => Ok(ip.to_string()),
        _ => Err("invalid DNS labels".into()),
    }
}
#[cfg(unix)]
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !matches!(a, 0 | 10 | 127 | 224..=255)
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 169 && b == 254)
                && !(a == 172 && (16..=31).contains(&b))
                && !(a == 192 && (b == 168 || b == 0 || b == 2))
                && !(a == 198 && (b == 18 || b == 19 || b == 51 && c == 100))
                && !(a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            // Only ordinary global unicast, excluding documentation/benchmark,
            // transition mechanisms and all loopback/link-local/ULA/multicast.
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
                && segments[0] != 0x2002
        }
    }
}

struct CombinedAuthorizer(
    Arc<dyn excalibur_types::runtime::NetworkAuthorizer>,
    Arc<dyn excalibur_types::runtime::NetworkAuthorizer>,
);
struct CombinedAdmission(
    Box<dyn excalibur_types::runtime::NetworkAdmission>,
    Box<dyn excalibur_types::runtime::NetworkAdmission>,
);
impl excalibur_types::runtime::NetworkAdmission for CombinedAdmission {
    fn revalidate(&self) -> Result<(), String> {
        self.0.revalidate()?;
        self.1.revalidate()
    }
}
impl excalibur_types::runtime::NetworkAuthorizer for CombinedAuthorizer {
    fn authorize_connection(
        &self,
        domain: &str,
        port: u16,
    ) -> Result<Box<dyn excalibur_types::runtime::NetworkAdmission>, String> {
        Ok(Box::new(CombinedAdmission(
            self.0.authorize_connection(domain, port)?,
            self.1.authorize_connection(domain, port)?,
        )))
    }

    fn authorize(&self, domain: &str, port: u16) -> Result<(), String> {
        self.0.authorize(domain, port)?;
        self.1.authorize(domain, port)
    }
    fn rejected(&self, domain: &str, port: u16, reason: &str) -> Result<(), String> {
        self.0.rejected(domain, port, reason)?;
        self.1.rejected(domain, port, reason)
    }
}
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::HostProxy;
