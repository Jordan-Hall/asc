//! The network guard: the one way the server connects to an address a user supplied
//! (docs/BUILDOUT.md section 2; bring-your-own storage and AI keys use nothing else).
//!
//! A user-supplied URL is an invitation to make the server call something the user cannot
//! reach: a database on the private network, the cloud metadata service, the admin listener on
//! loopback. The guard closes that in four steps.
//!
//! 1. **Syntax.** [`Guard::parse_endpoint`] accepts `https` (plain `http` only under the
//!    operator's loopback opt-in), no user name, password, query or fragment, a port outside the
//!    short deny list, and a host that is not a known metadata name.
//! 2. **Resolution.** [`Guard::pin`] resolves the host itself and judges every address it gets
//!    ([`classify`]), by address and never by name: loopback, link-local, private, carrier-grade
//!    NAT, multicast, documentation and benchmark ranges, the IPv4-mapped, NAT64 and 6to4 forms
//!    of those, and the metadata addresses. One bad address refuses the endpoint, so a name that
//!    also points inwards cannot be used.
//! 3. **Pinning.** The [`Pinned`] result holds the vetted addresses and builds the HTTP client
//!    with exactly those addresses for the host. The client has no resolver of its own, so a
//!    second lookup cannot return something else (DNS rebinding); SNI and the certificate name
//!    stay the host.
//! 4. **Containment.** The client follows no redirects (a redirect is the other way to reach an
//!    address nobody checked), reads no proxy settings, and is bounded in time.
//!
//! Errors carry a [`BlockReason`] slug and nothing of the URL.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use url::{Host, Url};

/// Longest URL the guard looks at.
pub const MAX_URL_LEN: usize = 2048;
/// Addresses of one host kept for pinning.
const MAX_ADDRS: usize = 16;
/// Ports no user-supplied endpoint may name unless the operator allows them: services that
/// speak a protocol an S3 or AI client could be steered into (ssh, smtp, databases, caches,
/// docker, etcd, elasticsearch).
pub const DENIED_PORTS: [u16; 9] = [22, 25, 3306, 5432, 6379, 11211, 2375, 2379, 9200];
/// Host names of cloud metadata services. They are refused before any lookup; the addresses
/// they resolve to are refused by [`classify`] as well.
const METADATA_NAMES: [&str; 6] = [
    "metadata.google.internal",
    "metadata.goog",
    "metadata",
    "instance-data",
    "instance-data.ec2.internal",
    "metadata.platformequinix.com",
];
/// Total time of a request on the [`Profile::Bulk`] client (object transfers).
const BULK_TOTAL: Duration = Duration::from_secs(900);

/// `[netguard]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetGuardConfig {
    /// Lets a user-supplied endpoint be plain `http` on a loopback address (a local S3 for
    /// development and tests). Off by default; nothing else about the guard relaxes.
    pub allow_loopback_http: bool,
    /// With `allow_loopback_http`, the only loopback ports plain http may reach. Empty: any port
    /// (development); set it wherever the local service has a known port. https to a loopback
    /// address is never allowed.
    pub loopback_ports: Vec<u16>,
    /// Ports allowed in addition to the usual ones, overriding [`DENIED_PORTS`].
    pub allow_ports: Vec<u16>,
    /// Seconds to connect.
    pub connect_timeout_secs: u64,
    /// Seconds a request may take (an API call), or may stay silent (a bulk transfer).
    pub request_timeout_secs: u64,
}

impl Default for NetGuardConfig {
    fn default() -> Self {
        Self {
            allow_loopback_http: false,
            loopback_ports: Vec::new(),
            allow_ports: Vec::new(),
            connect_timeout_secs: 10,
            request_timeout_secs: 30,
        }
    }
}

impl NetGuardConfig {
    /// Checks the bounds; the message names the key.
    pub fn validate(&self) -> Result<(), String> {
        if self.connect_timeout_secs == 0 || self.connect_timeout_secs > 300 {
            return Err("netguard.connect_timeout_secs must be 1 to 300".into());
        }
        if self.request_timeout_secs == 0 || self.request_timeout_secs > 3600 {
            return Err("netguard.request_timeout_secs must be 1 to 3600".into());
        }
        if self.allow_ports.contains(&0) || self.allow_ports.len() > 64 {
            return Err("netguard.allow_ports must hold at most 64 ports, 1 to 65535".into());
        }
        if self.loopback_ports.contains(&0) || self.loopback_ports.len() > 64 {
            return Err("netguard.loopback_ports must hold at most 64 ports, 1 to 65535".into());
        }
        if !self.loopback_ports.is_empty() && !self.allow_loopback_http {
            return Err("netguard.loopback_ports needs allow_loopback_http".into());
        }
        Ok(())
    }
}

/// Why an endpoint was refused. The slug is the `ErrorDetail.paths[0]` of
/// `STORAGE_ENDPOINT_BLOCKED` and `AI_ENDPOINT_BLOCKED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockReason {
    /// Not a URL the guard can read (not absolute, too long, control characters).
    Malformed,
    /// A scheme other than https, or http to a host that is not loopback under the opt-in.
    NotHttps,
    /// User name, password, query or fragment in the URL.
    CredentialsInUrl,
    /// A port outside 1-65535, or on the deny list.
    Port,
    /// The name did not resolve (or resolved to nothing).
    Unresolvable,
    Loopback,
    /// RFC 1918, unique-local (fc00::/7) and carrier-grade NAT (100.64.0.0/10).
    Private,
    LinkLocal,
    Metadata,
    /// Unspecified, multicast, broadcast, documentation, benchmark and other reserved ranges.
    Reserved,
}

impl BlockReason {
    /// The stable machine name.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::NotHttps => "not_https",
            Self::CredentialsInUrl => "credentials_in_url",
            Self::Port => "port",
            Self::Unresolvable => "unresolvable",
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link_local",
            Self::Metadata => "metadata",
            Self::Reserved => "reserved",
        }
    }

    /// A short fixed sentence for the user. It never repeats the URL.
    pub fn message(self) -> &'static str {
        match self {
            Self::Malformed => "the endpoint is not a valid absolute URL",
            Self::NotHttps => "the endpoint must use https",
            Self::CredentialsInUrl => {
                "the endpoint must not carry a user name, password, query or fragment"
            }
            Self::Port => "the endpoint port is not allowed",
            Self::Unresolvable => "the endpoint host does not resolve",
            Self::Loopback => "the endpoint resolves to this machine",
            Self::Private => "the endpoint resolves to a private network address",
            Self::LinkLocal => "the endpoint resolves to a link-local address",
            Self::Metadata => "the endpoint resolves to a cloud metadata address",
            Self::Reserved => "the endpoint resolves to a reserved address",
        }
    }
}

/// An endpoint the guard refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("endpoint blocked: {}", .0.slug())]
pub struct Blocked(pub BlockReason);

impl Blocked {
    pub fn reason(self) -> BlockReason {
        self.0
    }
}

// ------------------------------------------------------------------------------ classification

fn v4_reason(ip: Ipv4Addr) -> Option<BlockReason> {
    let o = ip.octets();
    // The metadata addresses first: the most specific reason wins.
    if ip == Ipv4Addr::new(169, 254, 169, 254)
        || ip == Ipv4Addr::new(100, 100, 100, 200)
        || ip == Ipv4Addr::new(169, 254, 170, 2)
    {
        return Some(BlockReason::Metadata);
    }
    if o[0] == 127 {
        return Some(BlockReason::Loopback);
    }
    if o[0] == 169 && o[1] == 254 {
        return Some(BlockReason::LinkLocal);
    }
    let private = o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 100 && (64..=127).contains(&o[1]));
    if private {
        return Some(BlockReason::Private);
    }
    let reserved = o[0] == 0 // "this network", unspecified
        || o[0] >= 224 // multicast, class E, broadcast
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // documentation
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // documentation
        || (o[0] == 203 && o[1] == 0 && o[2] == 113); // documentation
    reserved.then_some(BlockReason::Reserved)
}

fn v6_reason(ip: Ipv6Addr) -> Option<BlockReason> {
    let s = ip.segments();
    if ip == "fd00:ec2::254".parse::<Ipv6Addr>().ok()? {
        return Some(BlockReason::Metadata);
    }
    if ip.is_loopback() {
        return Some(BlockReason::Loopback);
    }
    if ip.is_unspecified() {
        return Some(BlockReason::Reserved);
    }
    // IPv4-mapped (::ffff:a.b.c.d): the IPv4 rules apply to the embedded address.
    if s[..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff {
        return v4_reason(embedded_v4(s[6], s[7]));
    }
    // IPv4-compatible (::a.b.c.d, deprecated) and anything else in ::/8 is not a host address.
    if s[0] >> 8 == 0 && !(s[0] == 0x64 && s[1] == 0xff9b) {
        return Some(BlockReason::Reserved);
    }
    // NAT64 well-known prefix 64:ff9b::/96 translates to the embedded IPv4 address.
    if s[0] == 0x64 && s[1] == 0xff9b {
        if s[2..6] == [0, 0, 0, 0] {
            return v4_reason(embedded_v4(s[6], s[7]));
        }
        // 64:ff9b:1::/48, local-use translation: not for the public internet.
        return Some(BlockReason::Reserved);
    }
    // 6to4 (2002::/16) embeds the IPv4 address in bits 16-47.
    if s[0] == 0x2002 {
        return v4_reason(embedded_v4(s[1], s[2]));
    }
    if s[0] & 0xffc0 == 0xfe80 {
        return Some(BlockReason::LinkLocal);
    }
    // Unique-local fc00::/7 and the deprecated site-local fec0::/10.
    if s[0] & 0xfe00 == 0xfc00 || s[0] & 0xffc0 == 0xfec0 {
        return Some(BlockReason::Private);
    }
    // Only global unicast, 2000::/3, is a public address; then take out the special-purpose
    // blocks inside it: IETF assignments (2001::/23, which holds Teredo and ORCHID), the
    // documentation prefixes 2001:db8::/32 and 3fff::/20.
    if s[0] & 0xe000 != 0x2000 {
        return Some(BlockReason::Reserved);
    }
    let special =
        (s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8)) || (s[0] == 0x3fff && s[1] < 0x1000);
    special.then_some(BlockReason::Reserved)
}

fn embedded_v4(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
}

/// Why `ip` is not a public address, or `None` when it is.
///
/// ```
/// use asc_serve::netguard::{classify, BlockReason};
/// assert_eq!(classify("10.1.2.3".parse().unwrap()), Some(BlockReason::Private));
/// assert_eq!(classify("::ffff:127.0.0.1".parse().unwrap()), Some(BlockReason::Loopback));
/// assert_eq!(classify("93.184.216.34".parse().unwrap()), None);
/// ```
pub fn classify(ip: IpAddr) -> Option<BlockReason> {
    match ip {
        IpAddr::V4(v) => v4_reason(v),
        IpAddr::V6(v) => v6_reason(v),
    }
}

/// Whether `ip` is a loopback address in any of its forms (127/8, ::1, ::ffff:127/8).
pub fn is_loopback(ip: IpAddr) -> bool {
    classify(ip) == Some(BlockReason::Loopback)
}

// ------------------------------------------------------------------------------ resolution

/// Name resolution, replaceable so tests can script what DNS answers.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// The addresses of `host`, or an error. The port is the caller's.
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
}

/// The system resolver (`getaddrinfo` through tokio).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        let addrs = tokio::net::lookup_host((host, port)).await?;
        Ok(addrs.map(|a| a.ip()).collect())
    }
}

/// A syntactically accepted endpoint, not yet resolved.
#[derive(Clone, Debug)]
pub struct Endpoint {
    url: Url,
    port: u16,
}

impl Endpoint {
    /// The URL without a trailing slash, as the S3 builder wants it.
    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn is_https(&self) -> bool {
        self.url.scheme() == "https"
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `https://host[:port]` plus the path, without a trailing slash.
    pub fn as_string(&self) -> String {
        self.url.as_str().trim_end_matches('/').to_owned()
    }
}

/// How strictly the URL itself is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// No query, no fragment, no credentials (a storage endpoint).
    Endpoint,
    /// A query is allowed (an API URL); credentials and fragments still are not.
    Url,
}

/// Which client to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// An API call: `request_timeout_secs` covers the whole request.
    Api,
    /// An object transfer: `request_timeout_secs` is the longest silence, the total is longer.
    Bulk,
}

/// The guard. Cheap to clone; share one.
#[derive(Clone)]
pub struct Guard {
    cfg: Arc<NetGuardConfig>,
    resolver: Arc<dyn Resolver>,
    /// Admits addresses in the private ranges (RFC 1918, unique-local, carrier-grade NAT) and nothing else that is
    /// inward. Not a file setting of the guard: it is set on the copy a feature hands to one caller
    /// ([`Guard::allowing_private`]), so a feature that does not ask for it cannot reach a private address.
    allow_private: bool,
}

impl fmt::Debug for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guard")
            .field("cfg", &self.cfg)
            .field("allow_private", &self.allow_private)
            .finish()
    }
}

impl Guard {
    /// A guard that resolves with the system resolver.
    pub fn new(cfg: NetGuardConfig) -> Self {
        Self::with_resolver(cfg, Arc::new(SystemResolver))
    }

    pub fn with_resolver(cfg: NetGuardConfig, resolver: Arc<dyn Resolver>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            resolver,
            allow_private: false,
        }
    }

    /// The same guard with the private ranges admitted when `on` (a self-hosted store on a LAN). Loopback, link-local,
    /// the metadata addresses and the reserved ranges stay refused, https stays required, and the original guard is
    /// unchanged.
    #[must_use]
    pub fn allowing_private(&self, on: bool) -> Self {
        Self {
            allow_private: on,
            ..self.clone()
        }
    }

    pub fn config(&self) -> &NetGuardConfig {
        &self.cfg
    }

    /// The syntax rules, with no lookup. Nothing is trusted yet.
    pub fn parse_endpoint(&self, raw: &str, shape: Shape) -> Result<Endpoint, Blocked> {
        if raw.is_empty()
            || raw.len() > MAX_URL_LEN
            || raw.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(Blocked(BlockReason::Malformed));
        }
        if raw.contains('\\') {
            return Err(Blocked(BlockReason::Malformed));
        }
        let url = Url::parse(raw).map_err(|_| Blocked(BlockReason::Malformed))?;
        match url.scheme() {
            "https" => {}
            // Whether the host is loopback is judged after resolution.
            "http" if self.cfg.allow_loopback_http => {}
            _ => return Err(Blocked(BlockReason::NotHttps)),
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || (shape == Shape::Endpoint && url.query().is_some())
        {
            return Err(Blocked(BlockReason::CredentialsInUrl));
        }
        let port = url
            .port_or_known_default()
            .filter(|p| *p != 0)
            .ok_or(Blocked(BlockReason::Port))?;
        if DENIED_PORTS.contains(&port) && !self.cfg.allow_ports.contains(&port) {
            return Err(Blocked(BlockReason::Port));
        }
        match url.host() {
            None => return Err(Blocked(BlockReason::Malformed)),
            Some(Host::Domain(d)) => {
                let d = d.trim_end_matches('.').to_ascii_lowercase();
                if d.is_empty() {
                    return Err(Blocked(BlockReason::Malformed));
                }
                if METADATA_NAMES.contains(&d.as_str()) {
                    return Err(Blocked(BlockReason::Metadata));
                }
            }
            Some(_) => {}
        }
        Ok(Endpoint { url, port })
    }

    /// Judges one address under this guard's rules: `Ok` when the server may connect to it. A
    /// loopback address is admitted only for plain http (never https) and only on a configured
    /// port when `loopback_ports` is set.
    fn admit(&self, ip: IpAddr, endpoint: &Endpoint) -> Result<(), Blocked> {
        match classify(ip) {
            None => Ok(()),
            Some(BlockReason::Loopback)
                if self.cfg.allow_loopback_http
                    && !endpoint.is_https()
                    && (self.cfg.loopback_ports.is_empty()
                        || self.cfg.loopback_ports.contains(&endpoint.port)) =>
            {
                Ok(())
            }
            Some(BlockReason::Private) if self.allow_private => Ok(()),
            Some(r) => Err(Blocked(r)),
        }
    }

    /// Resolves the endpoint once, judges every address, and returns the vetted set.
    pub async fn pin(&self, raw: &str, shape: Shape) -> Result<Pinned, Blocked> {
        let endpoint = self.parse_endpoint(raw, shape)?;
        let (host, mut addrs): (String, Vec<IpAddr>) = match endpoint.url.host() {
            Some(Host::Ipv4(ip)) => (ip.to_string(), vec![IpAddr::V4(ip)]),
            Some(Host::Ipv6(ip)) => (ip.to_string(), vec![IpAddr::V6(ip)]),
            Some(Host::Domain(d)) => {
                let name = d.trim_end_matches('.').to_owned();
                let lookup = tokio::time::timeout(
                    Duration::from_secs(self.cfg.connect_timeout_secs),
                    self.resolver.resolve(&name, endpoint.port),
                )
                .await;
                match lookup {
                    Ok(Ok(found)) if !found.is_empty() => (name, found),
                    _ => return Err(Blocked(BlockReason::Unresolvable)),
                }
            }
            None => return Err(Blocked(BlockReason::Malformed)),
        };
        let mut seen = Vec::with_capacity(addrs.len());
        addrs.retain(|a| {
            let fresh = !seen.contains(a);
            seen.push(*a);
            fresh
        });
        addrs.truncate(MAX_ADDRS);
        // Every address, not the first: a name that also answers with an inward address is refused.
        for ip in &addrs {
            self.admit(*ip, &endpoint)?;
        }
        // Plain http is for loopback only; the addresses are already known to be allowed.
        if !endpoint.is_https() && !addrs.iter().all(|a| is_loopback(*a)) {
            return Err(Blocked(BlockReason::NotHttps));
        }
        Ok(Pinned {
            endpoint,
            host,
            addrs,
            cfg: self.cfg.clone(),
        })
    }
}

/// An endpoint with the addresses the guard vetted. Everything that connects for the endpoint
/// goes through [`client`](Self::client), which can reach only these addresses.
#[derive(Clone)]
pub struct Pinned {
    endpoint: Endpoint,
    host: String,
    addrs: Vec<IpAddr>,
    cfg: Arc<NetGuardConfig>,
}

impl fmt::Debug for Pinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pinned")
            .field("host", &self.host)
            .field("addrs", &self.addrs)
            .finish()
    }
}

/// Resolves nothing: the pinned client has an address for its one host and no other host to find.
struct NoDns;

impl reqwest::dns::Resolve for NoDns {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async {
            Err::<reqwest::dns::Addrs, _>(Box::<dyn std::error::Error + Send + Sync>::from(
                "name resolution is disabled",
            ))
        })
    }
}

impl Pinned {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The vetted addresses, in resolver order.
    pub fn addrs(&self) -> &[IpAddr] {
        &self.addrs
    }

    /// The host name the certificate is checked against.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// An HTTP client that connects to the vetted addresses only: no redirects, no proxy, no
    /// resolver, https only (unless the endpoint is loopback http under the opt-in), bounded in
    /// time.
    pub fn client(&self, profile: Profile) -> Result<reqwest::Client, reqwest::Error> {
        let request = Duration::from_secs(self.cfg.request_timeout_secs);
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(NoDns))
            .connect_timeout(Duration::from_secs(self.cfg.connect_timeout_secs))
            .https_only(self.endpoint.is_https())
            .pool_idle_timeout(Duration::from_secs(30))
            .user_agent("asc-serve");
        b = match profile {
            Profile::Api => b.timeout(request),
            Profile::Bulk => b.read_timeout(request).timeout(BULK_TOTAL),
        };
        if matches!(self.endpoint.url.host(), Some(Host::Domain(_))) {
            let socks: Vec<SocketAddr> = self
                .addrs
                .iter()
                .map(|ip| SocketAddr::new(*ip, self.endpoint.port))
                .collect();
            b = b.resolve_to_addrs(&self.host, &socks);
        }
        b.build()
    }

    /// The transport of an S3 store over this endpoint.
    pub fn connector(&self) -> Result<PinnedConnector, reqwest::Error> {
        Ok(PinnedConnector {
            client: self.client(Profile::Bulk)?,
        })
    }
}

/// An `object_store` transport over a pinned client.
#[derive(Clone)]
pub struct PinnedConnector {
    client: reqwest::Client,
}

impl fmt::Debug for PinnedConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PinnedConnector")
    }
}

impl object_store::client::HttpConnector for PinnedConnector {
    fn connect(
        &self,
        _options: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        Ok(object_store::client::HttpClient::new(self.client.clone()))
    }
}

/// A resolver that answers from a table; for tests of code that takes a [`Resolver`].
#[derive(Default)]
pub struct StaticResolver {
    answers: std::sync::Mutex<HashMap<String, Vec<Vec<IpAddr>>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl StaticResolver {
    /// `host` answers `first` once, then `later` forever (when given), to script a rebinding.
    pub fn script(&self, host: &str, answers: Vec<Vec<IpAddr>>) {
        if let Ok(mut m) = self.answers.lock() {
            m.insert(host.to_ascii_lowercase(), answers);
        }
    }

    /// Lookups made so far.
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Resolver for StaticResolver {
    async fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut m = self
            .answers
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?;
        match m.get_mut(&host.to_ascii_lowercase()) {
            Some(list) if list.len() > 1 => Ok(list.remove(0)),
            Some(list) if list.len() == 1 => Ok(list[0].clone()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such host",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn every_range_is_refused_in_every_form() {
        use BlockReason::*;
        let table: &[(&str, Option<BlockReason>)] = &[
            // v4
            ("127.0.0.1", Some(Loopback)),
            ("127.255.255.254", Some(Loopback)),
            ("10.0.0.1", Some(Private)),
            ("172.16.0.1", Some(Private)),
            ("172.31.255.255", Some(Private)),
            ("172.32.0.1", None),
            ("172.15.255.255", None),
            ("192.168.1.1", Some(Private)),
            ("100.64.0.1", Some(Private)),
            ("100.127.255.255", Some(Private)),
            ("100.128.0.1", None),
            ("169.254.1.1", Some(LinkLocal)),
            ("169.254.169.254", Some(Metadata)),
            ("100.100.100.200", Some(Metadata)),
            ("0.0.0.0", Some(Reserved)),
            ("0.1.2.3", Some(Reserved)),
            ("224.0.0.1", Some(Reserved)),
            ("239.255.255.255", Some(Reserved)),
            ("240.0.0.1", Some(Reserved)),
            ("255.255.255.255", Some(Reserved)),
            ("192.0.0.8", Some(Reserved)),
            ("192.0.2.1", Some(Reserved)),
            ("198.18.0.1", Some(Reserved)),
            ("198.19.255.255", Some(Reserved)),
            ("198.51.100.7", Some(Reserved)),
            ("203.0.113.9", Some(Reserved)),
            ("192.88.99.1", Some(Reserved)),
            ("93.184.216.34", None),
            ("8.8.8.8", None),
            ("1.1.1.1", None),
            // v6
            ("::1", Some(Loopback)),
            ("::", Some(Reserved)),
            ("fe80::1", Some(LinkLocal)),
            ("febf::1", Some(LinkLocal)),
            ("fc00::1", Some(Private)),
            ("fd12:3456::1", Some(Private)),
            ("fec0::1", Some(Private)),
            ("fd00:ec2::254", Some(Metadata)),
            ("ff02::1", Some(Reserved)),
            ("2001:db8::1", Some(Reserved)),
            ("3fff::1", Some(Reserved)),
            ("3fff:1000::1", None),
            ("2001::1", Some(Reserved)),
            ("100::1", Some(Reserved)),
            ("2606:4700:4700::1111", None),
            ("2a00:1450:4001:81b::200e", None),
            // mapped, NAT64, 6to4 and compatible forms
            ("::ffff:127.0.0.1", Some(Loopback)),
            ("::ffff:10.1.2.3", Some(Private)),
            ("::ffff:169.254.169.254", Some(Metadata)),
            ("::ffff:192.168.0.1", Some(Private)),
            ("::ffff:8.8.8.8", None),
            ("64:ff9b::7f00:1", Some(Loopback)),
            ("64:ff9b::a9fe:a9fe", Some(Metadata)),
            ("64:ff9b::808:808", None),
            ("64:ff9b:1::1", Some(Reserved)),
            ("2002:7f00:1::", Some(Loopback)),
            ("2002:a00:1::1", Some(Private)),
            ("2002:808:808::1", None),
            ("::7f00:1", Some(Reserved)),
            ("::10.0.0.1", Some(Reserved)),
        ];
        for (text, want) in table {
            assert_eq!(classify(ip(text)), *want, "{text}");
        }
    }

    fn guard(loopback: bool) -> (Guard, Arc<StaticResolver>) {
        let r = Arc::new(StaticResolver::default());
        let cfg = NetGuardConfig {
            allow_loopback_http: loopback,
            ..NetGuardConfig::default()
        };
        (Guard::with_resolver(cfg, r.clone()), r)
    }

    fn reason(g: &Guard, url: &str) -> BlockReason {
        g.parse_endpoint(url, Shape::Endpoint).unwrap_err().reason()
    }

    #[test]
    fn syntax_rules() {
        let (g, _) = guard(false);
        assert_eq!(reason(&g, "http://s3.example.com"), BlockReason::NotHttps);
        assert_eq!(reason(&g, "ftp://s3.example.com"), BlockReason::NotHttps);
        assert_eq!(reason(&g, "file:///etc/passwd"), BlockReason::NotHttps);
        assert_eq!(reason(&g, "s3.example.com"), BlockReason::Malformed);
        assert_eq!(reason(&g, ""), BlockReason::Malformed);
        assert_eq!(
            reason(&g, "https://a b.example.com"),
            BlockReason::Malformed
        );
        assert_eq!(reason(&g, "https://exa\\mple.com"), BlockReason::Malformed);
        assert_eq!(
            reason(&g, &format!("https://h.example.com/{}", "a".repeat(3000))),
            BlockReason::Malformed
        );
        for url in [
            "https://user@s3.example.com",
            "https://user:pw@s3.example.com",
            "https://:pw@s3.example.com",
            "https://s3.example.com/?X-Amz-Signature=abc",
            "https://s3.example.com/#frag",
        ] {
            assert_eq!(reason(&g, url), BlockReason::CredentialsInUrl, "{url}");
        }
        for port in DENIED_PORTS {
            assert_eq!(
                reason(&g, &format!("https://s3.example.com:{port}")),
                BlockReason::Port
            );
        }
        assert_eq!(reason(&g, "https://s3.example.com:0"), BlockReason::Port);
        for name in [
            "metadata.google.internal",
            "METADATA.GOOGLE.INTERNAL.",
            "instance-data",
        ] {
            assert_eq!(
                reason(&g, &format!("https://{name}/")),
                BlockReason::Metadata
            );
        }
        let ok = g
            .parse_endpoint("https://S3.Example.com:9000/base/", Shape::Endpoint)
            .unwrap();
        assert_eq!(ok.port(), 9000);
        assert_eq!(ok.as_string(), "https://s3.example.com:9000/base");
        assert!(
            g.parse_endpoint("https://s3.example.com/?a=b", Shape::Url)
                .is_ok()
        );
        assert!(
            g.parse_endpoint("https://s3.example.com/#a", Shape::Url)
                .is_err()
        );
    }

    #[test]
    fn loopback_ports_are_checked() {
        let on = NetGuardConfig {
            allow_loopback_http: true,
            loopback_ports: vec![9000],
            ..NetGuardConfig::default()
        };
        assert!(on.validate().is_ok());
        assert!(
            NetGuardConfig {
                loopback_ports: vec![0],
                ..on.clone()
            }
            .validate()
            .is_err()
        );
        let off = NetGuardConfig {
            allow_loopback_http: false,
            ..on
        };
        assert!(off.validate().is_err(), "ports without the opt-in");
    }

    #[test]
    fn allow_ports_overrides_the_deny_list() {
        let cfg = NetGuardConfig {
            allow_ports: vec![9200],
            ..NetGuardConfig::default()
        };
        let g = Guard::new(cfg);
        assert!(
            g.parse_endpoint("https://s3.example.com:9200", Shape::Endpoint)
                .is_ok()
        );
        assert_eq!(reason(&g, "https://s3.example.com:6379"), BlockReason::Port);
    }

    #[test]
    fn odd_ip_spellings_are_normalised_before_the_check() {
        let g = Guard::new(NetGuardConfig::default());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for url in [
            "https://2130706433/",      // decimal 127.0.0.1
            "https://0x7f.1/",          // hex and short forms
            "https://0177.0.0.1/",      // octal
            "https://[::ffff:7f00:1]/", // mapped, hex
            "https://[::1]/",
            "https://127.1/",
            "https://169.254.169.254/",
            "https://[fd00:ec2::254]/",
            "https://10.0.0.1:8443/",
        ] {
            let err = rt.block_on(g.pin(url, Shape::Endpoint)).unwrap_err();
            assert!(
                matches!(
                    err.reason(),
                    BlockReason::Loopback | BlockReason::Metadata | BlockReason::Private
                ),
                "{url}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn private_ranges_are_admitted_only_by_a_guard_that_was_asked_to() {
        let (g, r) = guard(false);
        let lan = g.allowing_private(true);
        r.script(
            "garage.lan",
            vec![vec![ip("192.168.1.20")], vec![ip("192.168.1.20")]],
        );
        assert_eq!(
            g.pin("https://garage.lan", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Private,
            "the default stays deny"
        );
        let pinned = lan
            .pin("https://garage.lan", Shape::Endpoint)
            .await
            .unwrap();
        assert!(format!("{pinned:?}").contains("192.168.1.20"));
        // the original guard is not changed by the copy, and a copy can be switched off again
        assert_eq!(
            g.pin("https://garage.lan", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Private
        );
        assert!(!lan.allowing_private(false).allow_private);
        // everything else that is inward stays refused, whatever name leads there
        for (name, addr, want) in [
            ("loop.lan", "127.0.0.1", BlockReason::Loopback),
            ("meta.lan", "169.254.169.254", BlockReason::Metadata),
            ("link.lan", "169.254.1.1", BlockReason::LinkLocal),
            ("res.lan", "240.0.0.1", BlockReason::Reserved),
            ("mapped.lan", "::ffff:127.0.0.1", BlockReason::Loopback),
        ] {
            r.script(name, vec![vec![ip(addr)]]);
            assert_eq!(
                lan.pin(&format!("https://{name}"), Shape::Endpoint)
                    .await
                    .unwrap_err()
                    .reason(),
                want,
                "{name}"
            );
        }
        // one private and one metadata address: refused as a whole
        r.script("mix.lan", vec![vec![ip("10.0.0.7"), ip("169.254.169.254")]]);
        assert_eq!(
            lan.pin("https://mix.lan", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Metadata
        );
        // plain http to a private address is still not allowed
        assert_eq!(reason(&lan, "http://garage.lan"), BlockReason::NotHttps);
    }

    #[tokio::test]
    async fn one_bad_address_refuses_the_name() {
        let (g, r) = guard(false);
        r.script(
            "mixed.example.com",
            vec![vec![ip("93.184.216.34"), ip("10.0.0.7")]],
        );
        let err = g
            .pin("https://mixed.example.com", Shape::Endpoint)
            .await
            .unwrap_err();
        assert_eq!(err.reason(), BlockReason::Private);
        r.script("v6.example.com", vec![vec![ip("::ffff:169.254.169.254")]]);
        let err = g
            .pin("https://v6.example.com", Shape::Endpoint)
            .await
            .unwrap_err();
        assert_eq!(err.reason(), BlockReason::Metadata);
        r.script(
            "ok.example.com",
            vec![vec![
                ip("93.184.216.34"),
                ip("93.184.216.34"),
                ip("2606:4700:4700::1111"),
            ]],
        );
        let pinned = g
            .pin("https://ok.example.com", Shape::Endpoint)
            .await
            .unwrap();
        assert_eq!(pinned.addrs().len(), 2);
        assert_eq!(pinned.host(), "ok.example.com");
        let err = g
            .pin("https://gone.example.com", Shape::Endpoint)
            .await
            .unwrap_err();
        assert_eq!(err.reason(), BlockReason::Unresolvable);
        r.script("empty.example.com", vec![vec![]]);
        let err = g
            .pin("https://empty.example.com", Shape::Endpoint)
            .await
            .unwrap_err();
        assert_eq!(err.reason(), BlockReason::Unresolvable);
    }

    #[tokio::test]
    async fn loopback_needs_the_opt_in_and_http_is_loopback_only() {
        let (strict, r) = guard(false);
        r.script("local.example.com", vec![vec![ip("127.0.0.1")]]);
        assert_eq!(
            strict
                .pin("https://local.example.com", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Loopback
        );
        assert_eq!(
            strict
                .pin("https://127.0.0.1:9000", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Loopback
        );
        // The opt-in is for plain http: https to a loopback address stays refused, on any port.
        let (open, _) = guard(true);
        for url in [
            "https://127.0.0.1:9000",
            "https://127.0.0.1",
            "https://[::1]:9000",
        ] {
            assert_eq!(
                open.pin(url, Shape::Endpoint).await.unwrap_err().reason(),
                BlockReason::Loopback,
                "{url}"
            );
        }
        // With ports configured, only those ports are reachable on loopback.
        let ported = Guard::with_resolver(
            NetGuardConfig {
                allow_loopback_http: true,
                loopback_ports: vec![9000],
                ..NetGuardConfig::default()
            },
            Arc::new(StaticResolver::default()),
        );
        assert!(
            ported
                .pin("http://127.0.0.1:9000", Shape::Endpoint)
                .await
                .is_ok()
        );
        for url in [
            "http://127.0.0.1:9001",
            "http://[::1]:80",
            "http://127.0.0.1",
        ] {
            assert_eq!(
                ported.pin(url, Shape::Endpoint).await.unwrap_err().reason(),
                BlockReason::Loopback,
                "{url}"
            );
        }
        let (open, r) = guard(true);
        r.script("local.example.com", vec![vec![ip("127.0.0.1")]]);
        assert!(
            open.pin("http://local.example.com:9000", Shape::Endpoint)
                .await
                .is_ok()
        );
        assert!(
            open.pin("http://127.0.0.1:9000", Shape::Endpoint)
                .await
                .is_ok()
        );
        assert!(open.pin("http://[::1]:9000", Shape::Endpoint).await.is_ok());
        // The opt-in relaxes nothing else.
        assert_eq!(
            open.pin("http://10.0.0.1:9000", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Private
        );
        assert_eq!(
            open.pin("http://169.254.169.254", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::Metadata
        );
        r.script("public.example.com", vec![vec![ip("93.184.216.34")]]);
        assert_eq!(
            open.pin("http://public.example.com", Shape::Endpoint)
                .await
                .unwrap_err()
                .reason(),
            BlockReason::NotHttps
        );
    }

    /// A one-route HTTP server on loopback; returns its port and a counter of requests served.
    async fn serve(app: axum::Router) -> (u16, tokio::task::JoinHandle<()>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let h = tokio::spawn(async move {
            axum::serve(l, app).await.ok();
        });
        (port, h)
    }

    #[tokio::test]
    async fn rebinding_cannot_change_where_a_pinned_client_connects() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { "ok" }
            }),
        );
        let (port, _srv) = serve(app).await;
        let (g, r) = guard(true);
        // First answer: the loopback server (allowed by the opt-in). Every later answer: a
        // private address. A client that looked the name up again would be sent there.
        r.script(
            "rebind.example.com",
            vec![vec![ip("127.0.0.1")], vec![ip("10.9.9.9")]],
        );
        let pinned = g
            .pin(
                &format!("http://rebind.example.com:{port}"),
                Shape::Endpoint,
            )
            .await
            .unwrap();
        assert_eq!(r.calls(), 1);
        let client = pinned.client(Profile::Api).unwrap();
        for _ in 0..3 {
            let resp = client
                .get(format!("http://rebind.example.com:{port}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert_eq!(r.calls(), 1, "the client never resolved the name again");
        // A fresh pin sees the rebound answer and refuses it.
        let err = g
            .pin(
                &format!("http://rebind.example.com:{port}"),
                Shape::Endpoint,
            )
            .await
            .unwrap_err();
        assert_eq!(err.reason(), BlockReason::Private);
        // The pinned client cannot be pointed at another host either.
        assert!(
            client
                .get(format!("http://other.example.com:{port}/"))
                .send()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        use axum::http::{StatusCode, header};
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                )
            }),
        );
        let (port, _srv) = serve(app).await;
        let (g, _) = guard(true);
        let pinned = g
            .pin(&format!("http://127.0.0.1:{port}"), Shape::Endpoint)
            .await
            .unwrap();
        let resp = pinned
            .client(Profile::Api)
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 302, "the redirect is returned, not followed");
    }

    #[tokio::test]
    async fn http_is_refused_by_the_client_unless_the_endpoint_is_loopback_http() {
        let (g, r) = guard(false);
        r.script("s3.example.com", vec![vec![ip("93.184.216.34")]]);
        let pinned = g
            .pin("https://s3.example.com", Shape::Endpoint)
            .await
            .unwrap();
        let err = pinned
            .client(Profile::Api)
            .unwrap()
            .get("http://s3.example.com/")
            .send()
            .await
            .unwrap_err();
        assert!(err.is_builder() || err.is_request(), "{err}");
    }

    #[test]
    fn config_bounds() {
        assert!(NetGuardConfig::default().validate().is_ok());
        let bad = NetGuardConfig {
            connect_timeout_secs: 0,
            ..NetGuardConfig::default()
        };
        assert!(bad.validate().is_err());
        let bad = NetGuardConfig {
            allow_ports: vec![0],
            ..NetGuardConfig::default()
        };
        assert!(bad.validate().is_err());
        let toml: Result<NetGuardConfig, _> =
            toml::from_str("allow_loopback_http = true\nbogus = 1");
        assert!(toml.is_err());
    }
}
