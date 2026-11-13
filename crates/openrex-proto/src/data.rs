//! Data connections: one WebSocket per stream. A terminal's stream carries
//! the bytes it produces, from the server to the client. A byte stream
//! (`rex.data.bytes.v1`) carries one file, in either direction.

use anyhow::{Context, Result};
use async_tungstenite::tungstenite::Message;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::{FutureExt, StreamExt};

use crate::endpoint::{Endpoint, Ws};

pub const TERMINAL_SUBPROTOCOL: &str = "rex.data.terminal.v1";
/// One file's bytes, for `upload_begin` and `download_begin`.
pub const BYTES_SUBPROTOCOL: &str = "rex.data.bytes.v1";
/// The most bytes one data message carries.
pub const MAX_CHUNK: usize = 64 * 1024;

const HELLO: u8 = 0x01;
const RESUME: u8 = 0x02;
const WELCOME: u8 = 0x81;
const REJECT: u8 = 0x82;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("data handshake rejected: {}", reject_reason(*.0))]
    Rejected(u8),
    /// The block closed. Don't reconnect.
    #[error("the block closed")]
    BlockClosed,
    /// The stream is gone for good on this ticket. Get a new one.
    #[error("the data stream ended: {0}")]
    Ended(String),
}

fn reject_reason(code: u8) -> &'static str {
    match code {
        0x01 => "the ticket or token is unknown, expired, or already used",
        0x02 => "this client already has a connection to that data source",
        0x03 => "the stream isn't available",
        0x04 => "a limit was reached",
        0x05 => "the handshake was malformed or late",
        0x06 => "the server failed",
        0x07 => "the ticket was issued for a different subprotocol",
        _ => "unknown reason",
    }
}

/// An open stream of one block's output.
pub struct DataConn {
    ws: Ws,
    endpoint: Endpoint,
    subprotocol: String,
    /// Marks Rex's own messages in a terminal stream. Never zero.
    pub nonce: u64,
    reconnect_token: [u8; 16],
    /// How long the server keeps the stream for a resume, in milliseconds.
    pub keep_ms: u32,
    /// Stream bytes handed to the caller so far, across every connection.
    received: u64,
}

impl DataConn {
    /// Opens a stream with a ticket from `data.connect` or `session.bootstrap`.
    pub async fn open(endpoint: &Endpoint, subprotocol: &str, ticket: &str) -> Result<DataConn> {
        let ticket = URL_SAFE_NO_PAD
            .decode(ticket.trim_end_matches('='))
            .context("data ticket is not base64url")?;
        anyhow::ensure!(ticket.len() == 16, "data ticket is {} bytes, not 16", ticket.len());
        let mut hello = Vec::with_capacity(17);
        hello.push(HELLO);
        hello.extend_from_slice(&ticket);
        let (ws, welcome) = handshake(endpoint, subprotocol, hello).await?;
        Ok(DataConn {
            ws,
            endpoint: endpoint.clone(),
            subprotocol: subprotocol.to_string(),
            nonce: welcome.nonce,
            reconnect_token: welcome.token,
            keep_ms: welcome.keep_ms,
            received: 0,
        })
    }

    /// Continues the stream on a new connection after the old one dropped.
    pub async fn resume(&mut self) -> Result<()> {
        let mut msg = Vec::with_capacity(25);
        msg.push(RESUME);
        msg.extend_from_slice(&self.reconnect_token);
        msg.extend_from_slice(&self.received.to_le_bytes());
        let (ws, welcome) = handshake(&self.endpoint, &self.subprotocol, msg).await?;
        self.ws = ws;
        self.reconnect_token = welcome.token;
        self.keep_ms = welcome.keep_ms;
        Ok(())
    }

    /// The next piece of the stream. `Ok(None)` means the stream ended
    /// normally. Message boundaries have no meaning.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let Some(msg) = self.ws.next().await else {
                anyhow::bail!("data connection dropped");
            };
            match msg? {
                Message::Binary(b) => {
                    self.received += b.len() as u64;
                    return Ok(Some(b.to_vec()));
                }
                Message::Close(frame) => {
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        .unwrap_or((1005, String::new()));
                    return match code {
                        1000 => Ok(None),
                        4000 => Err(DataError::BlockClosed.into()),
                        // A keepalive timeout leaves the stream waiting for a resume.
                        1001 if reason == "keepalive timeout" => {
                            Err(anyhow::anyhow!("data connection dropped: {reason}"))
                        }
                        _ => Err(DataError::Ended(format!("{code} {reason}")).into()),
                    };
                }
                _ => {}
            }
        }
    }

    /// Ends the stream.
    pub async fn close(mut self) {
        self.ws.close(None).await.ok();
    }
}

/// How many times a download picks up again after its connection drops.
const RESUMES: u32 = 3;

/// One file on a `rex.data.bytes.v1` stream: a download's bytes, which the
/// server sends, or an upload's, which the client sends.
///
/// The sender ends the bytes with an empty message, a half-close. The server
/// then ends the stream with a close code, which is 1000 only when the
/// transfer worked: for an upload, once the file arrived and, for a batch,
/// once every file of it did and their paths were typed.
pub struct ByteStream {
    conn: DataConn,
    /// The server half-closed: the download has every byte.
    half_closed: bool,
    /// The server ended the stream with 1000.
    finished: bool,
    /// The client sent bytes: the stream is an upload. A resume only counts
    /// the bytes the server sent, so an upload can't be picked up again.
    wrote: bool,
}

impl ByteStream {
    /// Opens the stream with a ticket from `upload_begin` or `download_begin`.
    pub async fn open(endpoint: &Endpoint, ticket: &str) -> Result<ByteStream> {
        let conn = DataConn::open(endpoint, BYTES_SUBPROTOCOL, ticket).await?;
        Ok(ByteStream { conn, half_closed: false, finished: false, wrote: false })
    }

    /// The next bytes of a download, or `None` once the server has sent
    /// them all. A dropped connection is resumed where it stopped.
    pub async fn read(&mut self) -> Result<Option<Vec<u8>>> {
        if self.half_closed || self.finished {
            return Ok(None);
        }
        let mut resumes = 0;
        loop {
            match self.conn.next().await {
                Ok(Some(b)) if b.is_empty() => {
                    self.half_closed = true;
                    return Ok(None);
                }
                Ok(Some(b)) => return Ok(Some(b)),
                // Ended without a half-close, but with `ok`: nothing more comes.
                Ok(None) => {
                    self.finished = true;
                    return Ok(None);
                }
                Err(e) if e.downcast_ref::<DataError>().is_none() && resumes < RESUMES => {
                    resumes += 1;
                    self.conn.resume().await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends bytes of an upload, in messages of at most [`MAX_CHUNK`].
    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.wrote = true;
        for chunk in bytes.chunks(MAX_CHUNK) {
            self.check_incoming()?;
            self.conn.ws.send(Message::binary(chunk.to_vec())).await?;
        }
        Ok(())
    }

    /// Says that every byte of the upload has been sent.
    pub async fn close_write(&mut self) -> Result<()> {
        self.wrote = true;
        self.conn.ws.send(Message::binary(Vec::new())).await?;
        Ok(())
    }

    /// Waits for the server to end the stream. Succeeds only if it ended
    /// with `ok`. A download whose connection drops is resumed; an upload's
    /// is not, and fails.
    pub async fn finish(mut self) -> Result<()> {
        let mut resumes = 0;
        while !self.finished {
            match self.conn.next().await {
                Ok(Some(b)) if b.is_empty() => self.half_closed = true,
                Ok(Some(_)) if self.half_closed => anyhow::bail!("the server sent bytes after its half-close"),
                // An upload's stream carries nothing from the server, and the
                // rest of a download that wasn't read is dropped.
                Ok(Some(_)) => {}
                Ok(None) => self.finished = true,
                Err(e) if !self.wrote && e.downcast_ref::<DataError>().is_none() && resumes < RESUMES => {
                    resumes += 1;
                    self.conn.resume().await?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Reads what has already arrived without waiting, so that pings are
    /// answered while an upload only writes, and a stream the server ended
    /// early stops the upload.
    fn check_incoming(&mut self) -> Result<()> {
        while let Some(msg) = self.conn.ws.next().now_or_never() {
            match msg {
                None => anyhow::bail!("data connection dropped"),
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        .unwrap_or((1005, String::new()));
                    return Err(DataError::Ended(format!("{code} {reason}")).into());
                }
                Some(Ok(_)) => {}
            }
        }
        Ok(())
    }
}

/// A new data reference for `upload_begin` (`kind` is `upload`) or
/// `download_begin` (`download`): the kind, a colon, and a random UUID in
/// lower-case canonical form.
pub fn new_ref(kind: &str) -> String {
    format!("{kind}:{}", random_uuid())
}

/// A version 4 UUID. Its randomness comes from the standard library's
/// randomly keyed hasher, which is plenty for an ID that only has to be new.
pub fn random_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    for half in bytes.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        h.write_u128(now.as_nanos());
        half.copy_from_slice(&h.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

struct WelcomeMsg {
    token: [u8; 16],
    keep_ms: u32,
    nonce: u64,
}

async fn handshake(endpoint: &Endpoint, subprotocol: &str, first: Vec<u8>) -> Result<(Ws, WelcomeMsg)> {
    let mut ws = endpoint.websocket("/api/data", subprotocol).await?;
    ws.send(Message::binary(first)).await?;
    loop {
        let msg = ws
            .next()
            .await
            .context("server closed the data connection during the handshake")??;
        let Message::Binary(b) = msg else {
            if let Message::Close(frame) = msg {
                anyhow::bail!("data connection closed during the handshake: {frame:?}");
            }
            continue;
        };
        match b.first() {
            Some(&WELCOME) if b.len() == 32 => {
                let token: [u8; 16] = b[1..17].try_into().unwrap();
                let keep_ms = u32::from_le_bytes(b[20..24].try_into().unwrap());
                let nonce = u64::from_le_bytes(b[24..32].try_into().unwrap());
                return Ok((ws, WelcomeMsg { token, keep_ms, nonce }));
            }
            Some(&REJECT) => {
                return Err(DataError::Rejected(b.get(1).copied().unwrap_or(0)).into());
            }
            _ => anyhow::bail!("unexpected data handshake answer of {} bytes", b.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_net::unix::{UnixListener, UnixStream};
    use async_tungstenite::WebSocketStream;
    use async_tungstenite::tungstenite::handshake::server::{Request, Response};
    use async_tungstenite::tungstenite::http::HeaderValue;
    use async_tungstenite::tungstenite::protocol::CloseFrame;
    use async_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    use super::*;

    const TICKET: &str = "AAECAwQFBgcICQoLDA0ODw";
    const TOKEN: [u8; 16] = [7; 16];

    /// A socket path in a directory of its own, removed with it.
    struct Sock {
        dir: PathBuf,
        path: PathBuf,
    }

    impl Drop for Sock {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn socket(name: &str) -> Sock {
        let dir = std::env::temp_dir().join(format!("openrex-proto-{}", random_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        Sock { path: dir.join(name), dir }
    }

    /// Accepts one data connection and checks its first message.
    async fn accept(listener: &UnixListener, first: &[u8]) -> WebSocketStream<UnixStream> {
        let (io, _) = listener.accept().await.unwrap();
        let mut ws = async_tungstenite::accept_hdr_async(io, |_: &Request, mut resp: Response| {
            resp.headers_mut()
                .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(BYTES_SUBPROTOCOL));
            Ok(resp)
        })
        .await
        .unwrap();
        let Some(Ok(Message::Binary(hello))) = ws.next().await else { panic!("no handshake") };
        assert_eq!(&hello[..], first);
        let mut welcome = vec![WELCOME];
        welcome.extend_from_slice(&TOKEN);
        welcome.extend_from_slice(&[0, 0, 0]);
        welcome.extend_from_slice(&60_000u32.to_le_bytes());
        welcome.extend_from_slice(&9u64.to_le_bytes());
        ws.send(Message::binary(welcome)).await.unwrap();
        ws
    }

    fn hello() -> Vec<u8> {
        let mut m = vec![HELLO];
        m.extend(0u8..16);
        m
    }

    fn close(code: u16, reason: &str) -> Option<CloseFrame> {
        Some(CloseFrame { code: CloseCode::from(code), reason: reason.to_string().into() })
    }

    #[test]
    fn makes_refs_of_lower_case_uuids() {
        let r = new_ref("upload");
        let uuid = r.strip_prefix("upload:").unwrap();
        assert_eq!(uuid.len(), 36);
        assert!(uuid.chars().all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert!(matches!(uuid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(new_ref("upload"), r);
    }

    #[test]
    fn downloads_and_resumes_after_a_drop() {
        smol::block_on(async {
            let sock = socket("download.sock");
            let listener = UnixListener::bind(&sock.path).unwrap();
            let server = smol::spawn(async move {
                let mut ws = accept(&listener, &hello()).await;
                ws.send(Message::binary(b"hello ".to_vec())).await.unwrap();
                // Drop the connection without a close frame.
                drop(ws);
                let mut resume = vec![RESUME];
                resume.extend_from_slice(&TOKEN);
                resume.extend_from_slice(&6u64.to_le_bytes());
                let mut ws = accept(&listener, &resume).await;
                ws.send(Message::binary(b"world".to_vec())).await.unwrap();
                ws.send(Message::binary(Vec::new())).await.unwrap();
                ws.close(close(1000, "")).await.unwrap();
            });
            let mut stream = ByteStream::open(&Endpoint::Unix(sock.path.clone()), TICKET).await.unwrap();
            let mut got = Vec::new();
            while let Some(b) = stream.read().await.unwrap() {
                got.extend(b);
            }
            stream.finish().await.unwrap();
            assert_eq!(got, b"hello world");
            server.await;
        });
    }

    #[test]
    fn uploads_in_chunks_and_half_closes() {
        smol::block_on(async {
            let sock = socket("upload.sock");
            let listener = UnixListener::bind(&sock.path).unwrap();
            let server = smol::spawn(async move {
                let mut ws = accept(&listener, &hello()).await;
                let mut got = Vec::new();
                let mut messages = 0;
                loop {
                    let Some(Ok(Message::Binary(b))) = ws.next().await else { panic!("no half-close") };
                    if b.is_empty() {
                        break;
                    }
                    assert!(b.len() <= MAX_CHUNK);
                    messages += 1;
                    got.extend_from_slice(&b);
                }
                ws.close(close(1000, "")).await.unwrap();
                (got, messages)
            });
            let body: Vec<u8> = (0..MAX_CHUNK * 2 + 10).map(|i| i as u8).collect();
            let mut stream = ByteStream::open(&Endpoint::Unix(sock.path.clone()), TICKET).await.unwrap();
            stream.write(&body).await.unwrap();
            stream.close_write().await.unwrap();
            stream.finish().await.unwrap();
            let (got, messages) = server.await;
            assert_eq!((got == body, messages), (true, 3));
        });
    }

    #[test]
    fn fails_an_upload_whose_connection_drops() {
        smol::block_on(async {
            let sock = socket("dropped.sock");
            let listener = UnixListener::bind(&sock.path).unwrap();
            let server = smol::spawn(async move {
                let mut ws = accept(&listener, &hello()).await;
                while let Some(Ok(Message::Binary(b))) = ws.next().await {
                    if b.is_empty() {
                        break;
                    }
                }
                // Stop listening first, so that a resume would fail to connect.
                drop(listener);
                drop(ws);
            });
            let mut stream = ByteStream::open(&Endpoint::Unix(sock.path.clone()), TICKET).await.unwrap();
            stream.write(b"bytes").await.unwrap();
            stream.close_write().await.unwrap();
            let err = stream.finish().await.unwrap_err();
            assert!(!format!("{err:#}").contains("connecting"), "tried to resume: {err:#}");
            server.await;
        });
    }

    #[test]
    fn reports_a_failed_upload() {
        smol::block_on(async {
            let sock = socket("refused.sock");
            let listener = UnixListener::bind(&sock.path).unwrap();
            let server = smol::spawn(async move {
                let mut ws = accept(&listener, &hello()).await;
                while let Some(Ok(Message::Binary(b))) = ws.next().await {
                    if b.is_empty() {
                        break;
                    }
                }
                ws.close(close(4006, "data stream broke a rule")).await.ok();
            });
            let mut stream = ByteStream::open(&Endpoint::Unix(sock.path.clone()), TICKET).await.unwrap();
            stream.write(b"short").await.unwrap();
            stream.close_write().await.unwrap();
            let err = stream.finish().await.unwrap_err();
            assert!(format!("{err}").contains("4006 data stream broke a rule"), "{err}");
            server.await;
        });
    }
}
