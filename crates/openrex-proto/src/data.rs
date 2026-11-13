//! Data connections: one WebSocket per block, carrying the bytes the block
//! produces, from the server to the client.

use anyhow::{Context, Result};
use async_tungstenite::tungstenite::Message;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;

use crate::endpoint::{Endpoint, Ws};

pub const TERMINAL_SUBPROTOCOL: &str = "rex.data.terminal.v1";

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
