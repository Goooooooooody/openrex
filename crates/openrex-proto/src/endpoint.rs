use std::path::PathBuf;
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::http::HeaderValue;
use futures::io::{AsyncRead, AsyncWrite};

/// Where a Rex server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A local server's Unix socket.
    Unix(PathBuf),
    /// A server reached over plain TCP, such as one behind `--extra-listen`
    /// or a `tailnet://` listener.
    Tcp { host: String, port: u16 },
}

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
    /// `http://host:port` or `ws://host:port`.
    pub fn parse(s: &str) -> Result<Endpoint> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix://") {
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if s.starts_with('/') {
            return Ok(Endpoint::Unix(PathBuf::from(s)));
        }
        let rest = s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("ws://"))
            .unwrap_or(s);
        if rest.contains("://") {
            bail!("unsupported endpoint scheme in {s}");
        }
        let rest = rest.trim_end_matches('/');
        let (host, port) = rest
            .rsplit_once(':')
            .with_context(|| format!("endpoint {s} needs a port, such as host:7297"))?;
        let port: u16 = port
            .parse()
            .with_context(|| format!("endpoint {s} has an invalid port"))?;
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        Ok(Endpoint::Tcp {
            host: host.trim_matches(['[', ']']).to_string(),
            port,
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
            Endpoint::Tcp { host, port } => {
                let s = async_net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .with_context(|| format!("connecting to {host}:{port}"))?;
                s.set_nodelay(true).ok();
                Ok(Box::pin(s))
            }
        }
    }

    fn authority(&self) -> String {
        match self {
            // Over a Unix socket any host name works.
            Endpoint::Unix(_) => "localhost".to_string(),
            Endpoint::Tcp { host, port } if host.contains(':') => format!("[{host}]:{port}"),
            Endpoint::Tcp { host, port } => format!("{host}:{port}"),
        }
    }

    /// Opens a WebSocket at `path`, asking for `subprotocol`.
    pub async fn websocket(&self, path: &str, subprotocol: &str) -> Result<Ws> {
        let io = self.dial().await?;
        let url = format!("ws://{}{}", self.authority(), path);
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
            Endpoint::Tcp { host, port } => write!(f, "{host}:{port}"),
        }
    }
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
    }
}
