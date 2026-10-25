//! reqwest client construction (one client per worker, shared rustls configuration).

use std::sync::Arc;
use std::time::Duration;

use reqwest::redirect::Policy;

use crate::error::EngineError;
use crate::network::Network;
use crate::request::{HttpPreference, ProxyConfig};

/// Maximum redirects followed.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// Builds the base rustls configuration: aws-lc-rs provider + the OS trust store
/// (rustls-platform-verifier). Cloning it shares the TLS session cache, so workers of the same
/// download resume TLS sessions instead of doing full handshakes.
pub(crate) fn base_tls_config() -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_platform_verifier()
        .map_err(|e| e.to_string())
        .map(|b| b.with_no_client_auth())
}

/// Everything needed to build a client for a download.
#[derive(Clone)]
pub(crate) struct ClientSpec {
    pub proxy: ProxyConfig,
    pub http: HttpPreference,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub user_agent: String,
    pub tls: rustls::ClientConfig,
    /// Idle connections kept per host (1 for a segment worker's dedicated client).
    pub pool_max_idle: usize,
}

fn proxy_url(
    url: &str,
    username: &Option<String>,
    password: &Option<String>,
    socks: bool,
) -> Result<String, EngineError> {
    let invalid = |m: String| EngineError::InvalidUrl {
        message: format!("proxy: {m}"),
    };
    let with_scheme = if url.contains("://") {
        url.to_string()
    } else if socks {
        format!("socks5h://{url}")
    } else {
        format!("http://{url}")
    };
    let mut u = url::Url::parse(&with_scheme).map_err(|e| invalid(e.to_string()))?;
    // socks5:// would resolve DNS locally; socks5h:// lets the proxy resolve (no DNS leaks).
    if socks && u.scheme() == "socks5" {
        u = url::Url::parse(&with_scheme.replacen("socks5://", "socks5h://", 1))
            .map_err(|e| invalid(e.to_string()))?;
    }
    if let Some(user) = username {
        u.set_username(user)
            .map_err(|_| invalid("cannot set user name".into()))?;
        u.set_password(password.as_deref())
            .map_err(|_| invalid("cannot set password".into()))?;
    }
    Ok(u.to_string())
}

/// Builds a dedicated client. HTTP/1.1-only clients open exactly one connection per
/// concurrent request, which — with one client per worker — means one socket per worker,
/// reused across that worker's sequential range requests.
pub(crate) fn build_client(spec: &ClientSpec) -> Result<reqwest::Client, EngineError> {
    build_client_on(spec, None)
}

/// Like [`build_client`], with every connection pinned to `network` (see [`crate::network`]).
pub(crate) fn build_client_on(
    spec: &ClientSpec,
    network: Option<&Network>,
) -> Result<reqwest::Client, EngineError> {
    let mut tls = spec.tls.clone();
    tls.alpn_protocols = match spec.http {
        HttpPreference::Http1Only => vec![b"http/1.1".to_vec()],
        _ => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
    };
    let mut b = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(spec.connect_timeout)
        .read_timeout(spec.read_timeout)
        .pool_max_idle_per_host(spec.pool_max_idle.max(1))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(Policy::limited(MAX_REDIRECTS))
        .referer(false)
        .user_agent(spec.user_agent.clone());
    b = match spec.http {
        HttpPreference::Http1Only => b.http1_only(),
        HttpPreference::Auto => b,
        HttpPreference::Http3Experimental => {
            #[cfg(feature = "http3")]
            {
                b.http3_prior_knowledge()
            }
            #[cfg(not(feature = "http3"))]
            {
                b
            }
        }
    };
    b = match &spec.proxy {
        ProxyConfig::None => b.no_proxy(),
        ProxyConfig::System => b,
        ProxyConfig::Http {
            url,
            username,
            password,
        } => {
            let p = reqwest::Proxy::all(proxy_url(url, &None, &None, false)?).map_err(|e| {
                EngineError::InvalidUrl {
                    message: format!("proxy: {e}"),
                }
            })?;
            let p = match username {
                Some(u) => p.basic_auth(u, password.as_deref().unwrap_or("")),
                None => p,
            };
            b.proxy(p)
        }
        ProxyConfig::Socks5 {
            url,
            username,
            password,
        } => b.proxy(
            reqwest::Proxy::all(proxy_url(url, username, password, true)?).map_err(|e| {
                EngineError::InvalidUrl {
                    message: format!("proxy: {e}"),
                }
            })?,
        ),
    };
    if let Some(n) = network {
        b = bind_to_network(b, n);
    }
    b.build()
        .map_err(|e| EngineError::network(format!("cannot create HTTP client: {e}")))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bind_to_network(b: reqwest::ClientBuilder, n: &Network) -> reqwest::ClientBuilder {
    // Bound to the adapter itself; only connect to address families it has.
    let b = b.interface(n.interface());
    match n.families() {
        (true, true) | (false, false) => b,
        (v4, v6) => b.dns_resolver(Arc::new(crate::network::FamilyResolver { v4, v6 })),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn bind_to_network(b: reqwest::ClientBuilder, n: &Network) -> reqwest::ClientBuilder {
    // Routed by source address: bind it and only connect to addresses of its family.
    match n.local_addr() {
        Some(addr) => {
            b.local_address(addr)
                .dns_resolver(Arc::new(crate::network::FamilyResolver {
                    v4: addr.is_ipv4(),
                    v6: addr.is_ipv6(),
                }))
        }
        None => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_urls() {
        assert_eq!(
            proxy_url("proxy:3128", &None, &None, false).unwrap(),
            "http://proxy:3128/"
        );
        assert_eq!(
            proxy_url("socks5://h:1080", &None, &None, true).unwrap(),
            "socks5h://h:1080"
        );
        assert_eq!(
            proxy_url("h:1080", &Some("u".into()), &Some("p w".into()), true).unwrap(),
            "socks5h://u:p%20w@h:1080"
        );
        assert!(proxy_url("http://[bad", &None, &None, false).is_err());
    }

    #[test]
    fn clients_build_for_all_preferences() {
        let tls = base_tls_config().unwrap();
        for http in [
            HttpPreference::Http1Only,
            HttpPreference::Auto,
            HttpPreference::Http3Experimental,
        ] {
            for proxy in [
                ProxyConfig::None,
                ProxyConfig::System,
                ProxyConfig::Http {
                    url: "http://127.0.0.1:3128".into(),
                    username: Some("u".into()),
                    password: None,
                },
                ProxyConfig::Socks5 {
                    url: "127.0.0.1:1080".into(),
                    username: None,
                    password: None,
                },
            ] {
                let spec = ClientSpec {
                    proxy,
                    http,
                    connect_timeout: Duration::from_secs(5),
                    read_timeout: Duration::from_secs(5),
                    user_agent: "t".into(),
                    tls: tls.clone(),
                    pool_max_idle: 1,
                };
                build_client(&spec).unwrap();
            }
        }
    }
}
