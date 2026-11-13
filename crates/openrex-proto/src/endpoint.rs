use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::http::HeaderValue;
use futures::io::{AsyncRead, AsyncWrite};
use futures_rustls::pki_types::ServerName;
use futures_rustls::rustls::{ClientConfig, RootCertStore};

/// Where a Rex server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A local server's Unix socket.
    Unix(PathBuf),
    /// A server reached over plain TCP, such as one behind `--extra-listen`
    /// or a `tailnet://` listener.
    Tcp { host: String, port: u16 },
    /// A server reached over TLS, such as built-in Tailscale's HTTPS
    /// listener at `https://rex.tail1234.ts.net`.
    Tls { host: String, port: u16 },
}

/// The port Rex listens on when an address doesn't name one.
pub const DEFAULT_PORT: u16 = 7297;

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

pub type BoxIo = Pin<Box<dyn Io>>;
pub type Ws = WebSocketStream<BoxIo>;

impl Endpoint {
    /// The socket of the personal server for this account:
    /// `$XDG_RUNTIME_DIR/rex/server.sock`.
    pub fn default_local() -> Result<Endpoint> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is not set, so the local Rex socket can't be found")?;
        Ok(Endpoint::Unix(dir.join("rex").join("server.sock")))
    }

    /// Parses what `rex server status` prints on its `endpoint` line, and the
    /// forms `REX_SERVER` uses: `unix:///path`, a bare path, `host:port`,
    /// `http://host:port`, `ws://host:port`, or `https://host[:port]` (and
    /// `wss://`) for a server behind TLS.
    pub fn parse(s: &str) -> Result<Endpoint> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix://") {
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if s.starts_with('/') {
            return Ok(Endpoint::Unix(PathBuf::from(s)));
        }
        if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("wss://")) {
            let (host, port) = split_host_port(rest, s)?;
            return Ok(Endpoint::Tls { host, port: port.unwrap_or(443) });
        }
        let rest = s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("ws://"))
            .unwrap_or(s);
        if rest.contains("://") {
            bail!("unsupported endpoint scheme in {s}");
        }
        let (host, port) = split_host_port(rest, s)?;
        let port = port.with_context(|| format!("endpoint {s} needs a port, such as host:7297"))?;
        Ok(Endpoint::Tcp { host, port })
    }

    /// The endpoints a host's address may mean, in the order to try them,
    /// much as `rex --server` reads one: a bare name over HTTPS, then HTTPS
    /// and plain HTTP on Rex's port; `name:port` over HTTPS, then plain; an
    /// `https://` address without a port on 443, then on Rex's port. An
    /// address with a scheme and a port means exactly that.
    pub fn candidates(s: &str) -> Result<Vec<Endpoint>> {
        let s = s.trim();
        if s.is_empty() {
            bail!("the address is empty");
        }
        if s.starts_with("unix://") || s.starts_with('/') {
            return Ok(vec![Endpoint::parse(s)?]);
        }
        let tls = |host: &str, port| Endpoint::Tls { host: host.to_string(), port };
        let tcp = |host: &str, port| Endpoint::Tcp { host: host.to_string(), port };
        if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("wss://")) {
            return Ok(match split_host_port(rest, s)? {
                (host, Some(port)) => vec![tls(&host, port)],
                (host, None) => vec![tls(&host, 443), tls(&host, DEFAULT_PORT)],
            });
        }
        if let Some(rest) = s.strip_prefix("http://").or_else(|| s.strip_prefix("ws://")) {
            let (host, port) = split_host_port(rest, s)?;
            return Ok(vec![tcp(&host, port.unwrap_or(80))]);
        }
        if s.contains("://") {
            bail!("unsupported endpoint scheme in {s}");
        }
        Ok(match split_host_port(s, s)? {
            (host, Some(port)) => vec![tls(&host, port), tcp(&host, port)],
            (host, None) => vec![tls(&host, 443), tls(&host, DEFAULT_PORT), tcp(&host, DEFAULT_PORT)],
        })
    }

    /// The endpoint named by `REX_SERVER`, or the local socket.
    pub fn from_env() -> Result<Endpoint> {
        match std::env::var("REX_SERVER") {
            Ok(s) if !s.trim().is_empty() => Endpoint::parse(&s),
            _ => Endpoint::default_local(),
        }
    }

    async fn dial(&self) -> Result<BoxIo> {
        match self {
            Endpoint::Unix(path) => {
                let s = async_net::unix::UnixStream::connect(path)
                    .await
                    .with_context(|| format!("connecting to {}", path.display()))?;
                Ok(Box::pin(s))
            }
            Endpoint::Tcp { host, port } => Ok(Box::pin(tcp(host, *port).await?)),
            Endpoint::Tls { host, port } => {
                let s = tcp(host, *port).await?;
                let name = ServerName::try_from(host.clone())
                    .map_err(|_| anyhow::anyhow!("{host} is not a valid server name"))?;
                let s = futures_rustls::TlsConnector::from(tls_config()?)
                    .connect(name, s)
                    .await
                    .with_context(|| format!("TLS handshake with {host}:{port}"))?;
                Ok(Box::pin(s))
            }
        }
    }

    /// Whether something answers at the endpoint: the socket connects and,
    /// over TLS, the handshake succeeds. That tells HTTPS from plain HTTP on
    /// one port without making a client on the server.
    pub async fn probe(&self) -> Result<()> {
        self.dial().await.map(drop)
    }

    fn authority(&self) -> String {
        match self {
            // Over a Unix socket any host name works.
            Endpoint::Unix(_) => "localhost".to_string(),
            Endpoint::Tls { host, port: 443 } if host.contains(':') => format!("[{host}]"),
            Endpoint::Tls { host, port: 443 } => host.clone(),
            Endpoint::Tcp { host, port } | Endpoint::Tls { host, port } if host.contains(':') => {
                format!("[{host}]:{port}")
            }
            Endpoint::Tcp { host, port } | Endpoint::Tls { host, port } => format!("{host}:{port}"),
        }
    }

    /// Opens a WebSocket at `path`, asking for `subprotocol`.
    pub async fn websocket(&self, path: &str, subprotocol: &str) -> Result<Ws> {
        let io = self.dial().await?;
        let scheme = if matches!(self, Endpoint::Tls { .. }) { "wss" } else { "ws" };
        let url = format!("{scheme}://{}{}", self.authority(), path);
        let mut req = url.as_str().into_client_request()?;
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_str(subprotocol)?);
        let (ws, resp) = async_tungstenite::client_async(req, io)
            .await
            .with_context(|| format!("opening {url}"))?;
        let got = resp
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok());
        if got != Some(subprotocol) {
            bail!("server did not accept subprotocol {subprotocol} (answered {got:?})");
        }
        Ok(ws)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Unix(p) => write!(f, "unix://{}", p.display()),
            // Brackets around an IPv6 address, so the text parses back.
            Endpoint::Tcp { .. } => write!(f, "{}", self.authority()),
            Endpoint::Tls { .. } => write!(f, "https://{}", self.authority()),
        }
    }
}

async fn tcp(host: &str, port: u16) -> Result<async_net::TcpStream> {
    let s = async_net::TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    s.set_nodelay(true).ok();
    Ok(s)
}

/// The TLS settings every connection shares: the system's trusted
/// certificates, read once.
fn tls_config() -> Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            let found = rustls_native_certs::load_native_certs();
            let (added, _) = roots.add_parsable_certificates(found.certs);
            if added == 0 {
                return Err(format!(
                    "found no trusted certificates on this system ({} errors reading them)",
                    found.errors.len()
                ));
            }
            // Name the crypto provider rather than rely on a process-wide
            // default, which rustls would otherwise have to guess.
            let provider = Arc::new(futures_rustls::rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Splits `host`, `host:port`, or `[v6]:port`, ignoring a trailing `/`. A
/// bare IPv6 address has no port. `whole` is the address as given, for
/// error messages.
fn split_host_port(s: &str, whole: &str) -> Result<(String, Option<u16>)> {
    let s = s.trim_end_matches('/');
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .with_context(|| format!("endpoint {whole} has an unclosed [ in its host"))?;
        (host, after.strip_prefix(':'))
    } else {
        match s.split_once(':') {
            Some((_, rest)) if rest.contains(':') => (s, None),
            Some((host, port)) => (host, Some(port)),
            None => (s, None),
        }
    };
    let port = match port {
        Some(p) => Some(
            p.parse::<u16>()
                .with_context(|| format!("endpoint {whole} has an invalid port"))?,
        ),
        None => None,
    };
    let host = if host.is_empty() && port.is_some() { "127.0.0.1" } else { host };
    if host.is_empty() || host.contains(['/', ' ']) {
        bail!("endpoint {whole} has no valid host");
    }
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            Endpoint::parse("unix:///run/user/1000/rex/server.sock").unwrap(),
            Endpoint::Unix("/run/user/1000/rex/server.sock".into())
        );
        assert_eq!(
            Endpoint::parse("/tmp/x.sock").unwrap(),
            Endpoint::Unix("/tmp/x.sock".into())
        );
        assert_eq!(
            Endpoint::parse("http://box:7297/").unwrap(),
            Endpoint::Tcp { host: "box".into(), port: 7297 }
        );
        assert_eq!(
            Endpoint::parse(":7297").unwrap(),
            Endpoint::Tcp { host: "127.0.0.1".into(), port: 7297 }
        );
        assert!(Endpoint::parse("box").is_err());
        assert!(Endpoint::parse("quic://box:1").is_err());
        assert_eq!(
            Endpoint::parse("https://rex.tail1234.ts.net/").unwrap(),
            Endpoint::Tls { host: "rex.tail1234.ts.net".into(), port: 443 }
        );
        assert_eq!(
            Endpoint::parse("wss://[::1]:8443").unwrap(),
            Endpoint::Tls { host: "::1".into(), port: 8443 }
        );
        assert_eq!(
            Endpoint::parse("[::1]:7297").unwrap(),
            Endpoint::Tcp { host: "::1".into(), port: 7297 }
        );
    }

    #[test]
    fn host_addresses_expand_like_rex_server() {
        let tls = |h: &str, port| Endpoint::Tls { host: h.into(), port };
        let tcp = |h: &str, port| Endpoint::Tcp { host: h.into(), port };
        let ts = "studio.tail1234.ts.net";
        assert_eq!(
            Endpoint::candidates(ts).unwrap(),
            vec![tls(ts, 443), tls(ts, 7297), tcp(ts, 7297)]
        );
        assert_eq!(Endpoint::candidates("box:7300").unwrap(), vec![tls("box", 7300), tcp("box", 7300)]);
        assert_eq!(Endpoint::candidates("https://box").unwrap(), vec![tls("box", 443), tls("box", 7297)]);
        assert_eq!(Endpoint::candidates("https://box:9").unwrap(), vec![tls("box", 9)]);
        assert_eq!(Endpoint::candidates("http://box:80").unwrap(), vec![tcp("box", 80)]);
        assert_eq!(
            Endpoint::candidates("unix:///tmp/rex.sock").unwrap(),
            vec![Endpoint::Unix("/tmp/rex.sock".into())]
        );
        assert!(Endpoint::candidates("").is_err());
        assert!(Endpoint::candidates("tailnet://box:1").is_err());
        assert!(Endpoint::candidates("box:notaport").is_err());
    }

    #[test]
    fn tls_endpoints_print_as_https_urls() {
        let e = Endpoint::Tls { host: "rex.ts.net".into(), port: 443 };
        assert_eq!(e.to_string(), "https://rex.ts.net");
        assert_eq!(Endpoint::parse(&e.to_string()).unwrap(), e);
        let e = Endpoint::Tls { host: "box".into(), port: 7297 };
        assert_eq!(e.to_string(), "https://box:7297");
        assert_eq!(Endpoint::parse(&e.to_string()).unwrap(), e);
    }
}
