//! The control connection: one WebSocket that carries every method call and
//! every event, as JSON text messages.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use async_tungstenite::tungstenite::Message;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::endpoint::{Endpoint, Ws};

/// The API revision this client was written against.
pub const API_REVISION: u32 = 13;
const SUBPROTOCOL: &str = "rex.control.v1";

/// What the client says about itself when it connects.
#[derive(Clone, Debug, Default)]
pub struct Hello {
    pub name: String,
    pub version: String,
    pub label: Option<String>,
    /// The token from the last welcome, to continue that client.
    pub reconnect_token: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Welcome {
    pub max_message_bytes: usize,
    pub client_id: Option<String>,
    pub reconnect_token: Option<String>,
    pub resumed: bool,
    pub api_revision: u32,
}

/// A failed method call.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RpcError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl RpcError {
    fn disconnected() -> Self {
        RpcError { code: "disconnected".into(), message: "the control connection closed".into(), status: 0 }
    }
}

/// An event the server sent without being asked.
#[derive(Clone, Debug)]
pub struct ControlEvent {
    pub seq: u64,
    /// The event's name, such as `session_created` or `session_event`.
    pub name: String,
    pub payload: Value,
}

type Pending = HashMap<String, oneshot::Sender<Result<(u64, Value), RpcError>>>;

struct Inner {
    out: async_channel::Sender<String>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
}

/// A handle for calling methods. Cheap to clone.
#[derive(Clone)]
pub struct Control {
    inner: Arc<Inner>,
}

/// A freshly opened control connection.
pub struct Connection {
    pub control: Control,
    pub welcome: Welcome,
    /// Events, in the order the server sent them.
    pub events: async_channel::Receiver<ControlEvent>,
    /// Runs the connection. Spawn it; it finishes when the connection ends.
    pub driver: BoxFuture<'static, Result<()>>,
}

pub async fn connect(endpoint: &Endpoint, hello: Hello) -> Result<Connection> {
    let mut ws = endpoint.websocket("/api/control", SUBPROTOCOL).await?;

    let mut client = json!({
        "kind": "app",
        "platform": "linux",
        "name": hello.name,
        "version": hello.version,
    });
    if let Some(label) = &hello.label {
        client["label"] = json!(label);
    }
    let mut msg = json!({
        "type": "hello",
        "api_revision": API_REVISION,
        "client": client,
    });
    if let Some(token) = &hello.reconnect_token {
        msg["reconnect_token"] = json!(token);
    }
    let text = msg.to_string();
    if text.len() >= 512 {
        bail!("hello is {} bytes, and the server allows fewer than 512", text.len());
    }
    ws.send(Message::text(text)).await?;

    let welcome = loop {
        let m = ws.next().await.context("server closed the connection before its welcome")??;
        match m {
            Message::Text(t) => break parse_welcome(t.as_str())?,
            Message::Close(frame) => bail!("server refused the connection: {frame:?}"),
            _ => continue,
        }
    };

    let (out_tx, out_rx) = async_channel::unbounded::<String>();
    let (ev_tx, ev_rx) = async_channel::unbounded::<ControlEvent>();
    let inner = Arc::new(Inner { out: out_tx, pending: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1) });
    let driver = drive(ws, out_rx, inner.clone(), ev_tx).boxed();
    Ok(Connection { control: Control { inner }, welcome, events: ev_rx, driver })
}

fn parse_welcome(text: &str) -> Result<Welcome> {
    let v: Value = serde_json::from_str(text).context("welcome is not JSON")?;
    if v["type"] != "welcome" || v["protocol"] != SUBPROTOCOL {
        bail!("unexpected first message from server: {text}");
    }
    Ok(Welcome {
        max_message_bytes: v["limits"]["max_message_bytes"].as_u64().unwrap_or(65536) as usize,
        client_id: v["client"]["id"].as_str().map(str::to_string),
        reconnect_token: v["client"]["reconnect_token"].as_str().map(str::to_string),
        resumed: v["client"]["resumed"].as_bool().unwrap_or(false),
        api_revision: v["api_revision"].as_u64().unwrap_or(0) as u32,
    })
}

async fn drive(
    ws: Ws,
    out_rx: async_channel::Receiver<String>,
    inner: Arc<Inner>,
    ev_tx: async_channel::Sender<ControlEvent>,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();

    let writer = async {
        while let Ok(text) = out_rx.recv().await {
            sink.send(Message::text(text)).await?;
        }
        sink.close(None).await.ok();
        anyhow::Ok(())
    };

    let reader = async {
        while let Some(msg) = stream.next().await {
            let text = match msg? {
                Message::Text(t) => t,
                Message::Close(frame) => {
                    let why = frame
                        .map(|f| format!("{} {}", u16::from(f.code), f.reason))
                        .unwrap_or_else(|| "no reason given".into());
                    return Err(anyhow!("server closed the control connection: {why}"));
                }
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            match v["type"].as_str() {
                Some("response") => handle_response(&inner, v),
                Some("event") => {
                    let seq = v["seq"].as_u64().unwrap_or(0);
                    let name = v["payload"]["type"].as_str().unwrap_or("").to_string();
                    let payload = v["payload"]["payload"].clone();
                    // A caller that only makes requests may have dropped the
                    // receiver. Its events have nowhere to go.
                    ev_tx.send(ControlEvent { seq, name, payload }).await.ok();
                }
                // A newer server may add message types.
                _ => {}
            }
        }
        anyhow::Ok(())
    };

    let result = futures::select! {
        r = writer.fuse() => r,
        r = reader.fuse() => r,
    };
    // Nothing will answer the requests that are still waiting.
    inner.pending.lock().unwrap().clear();
    inner.out.close();
    result
}

fn handle_response(inner: &Inner, mut v: Value) {
    let Some(id) = v["id"].as_str() else { return };
    let Some(tx) = inner.pending.lock().unwrap().remove(id) else {
        return;
    };
    let seq = v["seq"].as_u64().unwrap_or(0);
    let result = match v.get("error").filter(|e| !e.is_null()) {
        Some(e) => Err(RpcError {
            code: e["code"].as_str().unwrap_or("internal").to_string(),
            message: e["message"].as_str().unwrap_or("").to_string(),
            status: e["status"].as_u64().unwrap_or(500) as u16,
        }),
        None => Ok((seq, v["payload"].take())),
    };
    tx.send(result).ok();
}

impl Control {
    /// Calls a method and returns its result with the response's `seq`: the
    /// last event the result already includes.
    pub async fn call_seq(&self, method: &str, payload: Value) -> Result<(u64, Value), RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id.clone(), tx);
        let mut req = json!({"type": "request", "id": id, "method": method});
        if !payload.is_null() {
            req["payload"] = payload;
        }
        if self.inner.out.send(req.to_string()).await.is_err() {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(RpcError::disconnected());
        }
        rx.await.unwrap_or_else(|_| Err(RpcError::disconnected()))
    }

    pub async fn call(&self, method: &str, payload: Value) -> Result<Value, RpcError> {
        self.call_seq(method, payload).await.map(|(_, v)| v)
    }

    /// Calls a method and decodes its result.
    pub async fn call_as<T: DeserializeOwned>(&self, method: &str, payload: Value) -> Result<T> {
        let v = self.call(method, payload).await?;
        serde_json::from_value(v).with_context(|| format!("decoding the result of {method}"))
    }

    /// Calls a method of a block, such as `com.superlogical.terminal.write`.
    pub async fn block_call(
        &self,
        method: &str,
        session_id: &str,
        block_id: &str,
        args: Value,
    ) -> Result<Value, RpcError> {
        self.call(method, json!({"session_id": session_id, "block_id": block_id, "args": args})).await
    }

    /// Queues a request without waiting for its response. Used for input,
    /// where the server keeps the order the requests arrive in.
    pub fn send_nowait(&self, method: &str, payload: Value) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let req = json!({"type": "request", "id": id, "method": method, "payload": payload});
        self.inner.out.try_send(req.to_string()).ok();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.out.is_closed()
    }

    /// Closes the connection with a close frame, which ends this client.
    pub fn close(&self) {
        self.inner.out.close();
    }
}
