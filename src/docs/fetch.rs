//! Safely fetching the web pages a search returned.
//!
//! The URLs come from a search engine, so from the open internet, and Clicky
//! requests them from the user's own machine. A hostile search result could
//! point at something on the user's network (a router's admin page, a cloud
//! metadata address, `localhost`) and get its contents handed to the AI
//! provider. So every request, and every redirect hop, is checked:
//!
//! * `https://` only, on the default port, with no credentials in the URL;
//! * the host must not be an internal-looking name or a non-public IP address;
//! * names are resolved through a resolver that drops non-public addresses
//!   (this also defeats DNS rebinding, because the connection uses exactly the
//!   addresses we vetted);
//! * the body is size-capped and only text-like content types are read.
//!
//! Note: when an HTTP proxy is configured, the proxy resolves names, so the
//! address filter cannot apply; the URL checks above still do.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("the page answered HTTP {0}")]
    Status(u16),
    #[error("not a readable page ({0})")]
    UnsupportedContent(String),
}

/// Largest page body we will read.
pub const MAXIMUM_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAXIMUM_REDIRECTS: usize = 4;

// ---------------------------------------------------------------------------
// Which addresses are fine to connect to
// ---------------------------------------------------------------------------

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    let is_carrier_grade_nat = octets[0] == 100 && (64..=127).contains(&octets[1]);
    let is_ietf_protocol_assignment = octets[0] == 192 && octets[1] == 0 && octets[2] == 0;
    let is_benchmarking = octets[0] == 198 && (18..=19).contains(&octets[1]);
    let is_reserved = octets[0] >= 240; // includes the broadcast address
    !(octets[0] == 0
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local() // 169.254/16, which includes cloud metadata services
        || address.is_multicast()
        || address.is_documentation()
        || is_carrier_grade_nat
        || is_ietf_protocol_assignment
        || is_benchmarking
        || is_reserved)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let is_unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let is_link_local = (segments[0] & 0xffc0) == 0xfe80;
    let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    let is_teredo = segments[0] == 0x2001 && segments[1] == 0x0000;

    // Addresses that embed an IPv4 address are only as public as that address.
    if let Some(embedded) = address.to_ipv4_mapped() {
        return is_public_ipv4(embedded);
    }
    let is_nat64 = segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0];
    if is_nat64 {
        let [high, low] = [segments[6], segments[7]];
        let embedded = Ipv4Addr::new((high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8);
        return is_public_ipv4(embedded);
    }
    if segments[0] == 0x2002 {
        // 6to4: the IPv4 address sits in the next 32 bits.
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return is_public_ipv4(embedded);
    }

    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || is_unique_local
        || is_link_local
        || is_documentation
        || is_teredo)
}

/// True for addresses on the public internet. False for loopback, private
/// networks, link-local (including the `169.254.169.254` metadata address),
/// carrier-grade NAT, multicast, reserved and documentation ranges.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

// ---------------------------------------------------------------------------
// URL policy
// ---------------------------------------------------------------------------

/// Decides which URLs may be fetched.
#[derive(Clone, Debug, Default)]
pub struct UrlPolicy {
    /// `host:port` pairs that bypass every check. Only for tests and
    /// deliberate local setups; the default policy has none.
    exceptions: Vec<String>,
}

impl UrlPolicy {
    pub fn strict() -> Self {
        Self::default()
    }

    /// Allows one `host:port` even though it would normally be blocked.
    pub fn with_exception(mut self, host_and_port: impl Into<String>) -> Self {
        self.exceptions.push(host_and_port.into());
        self
    }

    fn is_exception(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host_and_port = format!("{host}:{}", url.port_or_known_default().unwrap_or(0));
        self.exceptions.contains(&host_and_port)
    }

    pub fn check_url(&self, url: &Url) -> Result<(), FetchError> {
        if self.is_exception(url) {
            return Ok(());
        }
        if url.scheme() != "https" {
            return Err(FetchError::Blocked(
                "only https:// pages are fetched".into(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(FetchError::Blocked(
                "the address contains a username or password".into(),
            ));
        }
        if url.port().is_some_and(|port| port != 443) {
            return Err(FetchError::Blocked("unusual port".into()));
        }
        match url.host() {
            None => Err(FetchError::Blocked("the address has no host".into())),
            Some(Host::Ipv4(address)) => check_literal_address(IpAddr::V4(address)),
            Some(Host::Ipv6(address)) => check_literal_address(IpAddr::V6(address)),
            Some(Host::Domain(domain)) => check_domain_name(domain),
        }
    }
}

fn check_literal_address(address: IpAddr) -> Result<(), FetchError> {
    if is_public_ip(address) {
        Ok(())
    } else {
        Err(FetchError::Blocked(
            "the address is not on the public internet".into(),
        ))
    }
}

fn check_domain_name(domain: &str) -> Result<(), FetchError> {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    const INTERNAL_SUFFIXES: [&str; 7] = [
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".home.arpa",
        ".corp",
        ".intranet",
    ];
    let looks_internal = domain == "localhost"
        || !domain.contains('.') // single-label names only resolve on private networks
        || INTERNAL_SUFFIXES.iter().any(|suffix| domain.ends_with(suffix));
    if looks_internal {
        Err(FetchError::Blocked(
            "the host name looks like an internal address".into(),
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DNS: only connect to public addresses
// ---------------------------------------------------------------------------

/// Resolves `host`, keeping only public addresses. Fails if none remain.
pub async fn resolve_public_addresses(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, FetchError> {
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| FetchError::Network(format!("could not resolve {host}: {error}")))?
        .collect();
    let total = resolved.len();
    let public: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|address| is_public_ip(address.ip()))
        .collect();
    if public.is_empty() {
        return Err(FetchError::Blocked(if total == 0 {
            format!("{host} did not resolve to any address")
        } else {
            format!("{host} resolves only to non-public addresses")
        }));
    }
    Ok(public)
}

struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses = resolve_public_addresses(name.as_str(), 0)
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

// ---------------------------------------------------------------------------
// The client and the fetch
// ---------------------------------------------------------------------------

/// A client that enforces `policy` on the first request and on every redirect,
/// and only connects to public addresses.
pub fn build_fetch_client(policy: &UrlPolicy) -> reqwest::Client {
    let redirect_policy = policy.clone();
    reqwest::Client::builder()
        .user_agent(concat!(
            "Mozilla/5.0 (compatible; Clicky/",
            env!("CARGO_PKG_VERSION"),
            ")"
        ))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAXIMUM_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match redirect_policy.check_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(error) => attempt.error(error.to_string()),
            }
        }))
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .build()
        .expect("static reqwest client configuration is valid")
}

#[derive(Debug, Clone)]
pub struct FetchedPage {
    pub final_url: String,
    pub content_type: String,
    pub body: String,
}

fn is_readable_content_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Servers sometimes omit the header; treat that as HTML and let extraction decide.
    essence.is_empty()
        || matches!(
            essence.as_str(),
            "text/html"
                | "application/xhtml+xml"
                | "text/plain"
                | "text/markdown"
                | "text/x-markdown"
        )
}

/// Turns a failed request into a [`FetchError`]. Rejections by our own redirect
/// policy and DNS resolver arrive wrapped inside reqwest's error, so the whole
/// chain of causes is read to find them.
fn describe_request_error(error: reqwest::Error) -> FetchError {
    let error = error.without_url(); // never put a URL (which may carry a token) in a message
    let mut messages = vec![error.to_string()];
    let mut cause = std::error::Error::source(&error);
    while let Some(next) = cause {
        messages.push(next.to_string());
        cause = next.source();
    }
    let full_message = messages.join(": ");
    match full_message.find("blocked: ") {
        Some(index) => FetchError::Blocked(full_message[index + "blocked: ".len()..].to_string()),
        None => FetchError::Network(full_message),
    }
}

pub async fn fetch_page(
    client: &reqwest::Client,
    policy: &UrlPolicy,
    url: &str,
) -> Result<FetchedPage, FetchError> {
    let url = Url::parse(url).map_err(|_| FetchError::Blocked("not a valid address".into()))?;
    policy.check_url(&url)?;

    let response = client
        .get(url)
        .header(
            "accept",
            "text/html,application/xhtml+xml,text/plain;q=0.8,*/*;q=0.1",
        )
        .header("accept-language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(describe_request_error)?;

    // The redirect policy already vetted every hop; check the landing page too.
    policy.check_url(response.url())?;

    if !response.status().is_success() {
        return Err(FetchError::Status(response.status().as_u16()));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !is_readable_content_type(&content_type) {
        return Err(FetchError::UnsupportedContent(content_type));
    }
    let final_url = response.url().to_string();

    let mut body: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| FetchError::Network(error.without_url().to_string()))?;
        let room = MAXIMUM_BODY_BYTES.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAXIMUM_BODY_BYTES {
            break; // the useful part of a docs page is near the top
        }
    }

    Ok(FetchedPage {
        final_url,
        content_type,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn public_addresses_are_allowed() {
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "151.101.1.69",
            "2606:4700:4700::1111",
            "2a00:1450:4001:81b::200e",
        ] {
            assert!(is_public_ip(address(public)), "{public}");
        }
    }

    #[test]
    fn private_loopback_and_metadata_addresses_are_blocked() {
        for blocked in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata service
            "100.64.0.1",      // carrier-grade NAT
            "0.0.0.0",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "2001:db8::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredo
            "::ffff:127.0.0.1",                     // IPv4-mapped loopback
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",  // NAT64 wrapping 127.0.0.1
            "2002:7f00:1::1",   // 6to4 wrapping 127.0.0.1
            "2002:c0a8:101::1", // 6to4 wrapping 192.168.1.1
        ] {
            assert!(!is_public_ip(address(blocked)), "{blocked} must be blocked");
        }
    }

    #[test]
    fn only_https_on_the_default_port_to_public_hosts_is_allowed() {
        let policy = UrlPolicy::strict();
        for allowed in [
            "https://docs.github.com/en/pull-requests",
            "https://help.figma.com/hc/en-us/articles/1",
            "https://doc.rust-lang.org:443/book/",
            "https://8.8.8.8/",
            "https://[2606:4700:4700::1111]/",
        ] {
            assert!(policy.check_url(&url(allowed)).is_ok(), "{allowed}");
        }
    }

    #[test]
    fn dangerous_urls_are_rejected() {
        let policy = UrlPolicy::strict();
        for blocked in [
            "http://docs.github.com/", // plain http can be tampered with
            "file:///etc/passwd",
            "ftp://example.com/file",
            "https://localhost/",
            "https://LOCALHOST./",
            "https://printer.local/",
            "https://router.lan/admin",
            "https://db.internal/",
            "https://intranet/", // single label
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://169.254.169.254/latest/meta-data/",
            "https://192.168.0.1/",
            "https://2130706433/", // decimal form of 127.0.0.1
            "https://0x7f.1/",     // hex shorthand
            "https://user:secret@example.com/",
            "https://example.com:8443/",
            "https://example.com:22/",
        ] {
            assert!(
                policy.check_url(&url(blocked)).is_err(),
                "{blocked} must be rejected"
            );
        }
    }

    #[test]
    fn exceptions_are_exact_host_and_port_matches() {
        let policy = UrlPolicy::strict().with_exception("127.0.0.1:5000");
        assert!(policy.check_url(&url("http://127.0.0.1:5000/page")).is_ok());
        assert!(
            policy
                .check_url(&url("http://127.0.0.1:5001/page"))
                .is_err()
        );
        assert!(
            policy
                .check_url(&url("http://localhost:5000/page"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn names_that_resolve_only_to_private_addresses_are_rejected() {
        // "localhost" resolves to loopback on every system.
        let error = resolve_public_addresses("localhost", 443)
            .await
            .unwrap_err();
        assert!(matches!(error, FetchError::Blocked(_)), "{error:?}");
    }

    #[tokio::test]
    async fn the_resolver_hook_refuses_localhost() {
        let name: Name = "localhost".parse().unwrap();
        let outcome = PublicOnlyResolver.resolve(name).await;
        assert!(outcome.is_err());
    }

    #[test]
    fn only_text_like_content_types_are_read() {
        for readable in [
            "text/html; charset=utf-8",
            "TEXT/HTML",
            "application/xhtml+xml",
            "text/plain",
            "text/markdown",
            "",
        ] {
            assert!(is_readable_content_type(readable), "{readable:?}");
        }
        for unreadable in [
            "application/pdf",
            "image/png",
            "application/octet-stream",
            "application/json",
            "video/mp4",
        ] {
            assert!(!is_readable_content_type(unreadable), "{unreadable:?}");
        }
    }
}
