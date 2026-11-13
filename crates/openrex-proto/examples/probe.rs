//! Connects to the local server, attaches to a session, opens the terminal
//! stream of its first block, and writes what arrives to a file.
//!
//!   cargo run --example probe -- <out-file> [seconds]

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::FutureExt;
use openrex_proto::control::{self, Hello};
use openrex_proto::data::{DataConn, TERMINAL_SUBPROTOCOL};
use openrex_proto::{Endpoint, StreamItem, StreamSplitter};
use serde_json::json;

fn main() -> Result<()> {
    let out = std::env::args().nth(1).context("usage: probe <out-file> [seconds]")?;
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    smol::block_on(run(out, secs))
}

async fn run(out: String, secs: u64) -> Result<()> {
    let endpoint = Endpoint::from_env()?;
    let conn = control::connect(
        &endpoint,
        Hello { name: "openrex-probe".into(), version: "0.0.0".into(), ..Default::default() },
    )
    .await?;
    println!("welcome: {:?}", conn.welcome);
    let ctl = conn.control.clone();
    smol::spawn(conn.driver).detach();
    let events = conn.events;
    smol::spawn(async move {
        while let Ok(ev) = events.recv().await {
            let p = ev.payload.to_string();
            println!("event #{} {} {}", ev.seq, ev.name, &p[..p.len().min(300)]);
        }
    })
    .detach();

    let boot = ctl
        .call(
            "session.bootstrap",
            json!({
                "event_scope": "all",
                "data_tickets": [{
                    "creator_name": "com.superlogical.terminal",
                    "data_ref_id": "terminal",
                    "subprotocol": TERMINAL_SUBPROTOCOL,
                }],
            }),
        )
        .await?;
    std::fs::write(format!("{out}.bootstrap.json"), serde_json::to_string_pretty(&boot)?)?;
    println!("bootstrap keys: {:?}", boot.as_object().map(|o| o.keys().collect::<Vec<_>>()));

    let sessions = ctl.call("session.list", json!({})).await?;
    println!("session.list: {sessions}");

    let session_id = sessions["sessions"][0]["session_id"]
        .as_str()
        .context("no sessions: run `rex session new probe` first")?
        .to_string();
    let view = ctl.call("session.attach", json!({"session_id": session_id})).await?;
    std::fs::write(format!("{out}.attach.json"), serde_json::to_string_pretty(&view)?)?;
    let blocks = ctl.call("session.list_blocks", json!({"session_id": session_id})).await?;
    println!("blocks: {blocks}");
    let block_id = blocks["blocks"][0]["block_id"].as_str().context("no blocks")?.to_string();

    let ticket = ctl
        .call(
            "data.connect",
            json!({
                "session_id": session_id,
                "block_id": block_id,
                "data_ref_id": "terminal",
                "subprotocol": TERMINAL_SUBPROTOCOL,
            }),
        )
        .await?;
    println!("ticket: expires_in_ms={}", ticket["expires_in_ms"]);
    let mut data = DataConn::open(&endpoint, TERMINAL_SUBPROTOCOL, ticket["ticket"].as_str().unwrap()).await?;
    println!("data welcome: nonce={} keep_ms={}", data.nonce, data.keep_ms);

    let mut raw = std::fs::File::create(&out)?;
    let mut splitter = StreamSplitter::new(data.nonce);
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut typed = false;
    let mut total = 0usize;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let chunk = futures::select! {
            c = data.next().fuse() => c?,
            _ = smol::Timer::after(left.min(Duration::from_millis(700))).fuse() => {
                if !typed {
                    typed = true;
                    let r = ctl.block_call(
                        "com.superlogical.terminal.write",
                        &session_id,
                        &block_id,
                        json!({"data": "printf 'hello \\033[31mred\\033[0m\\n'\n"}),
                    ).await?;
                    println!("write -> {r}");
                }
                continue;
            }
        };
        let Some(chunk) = chunk else { break };
        total += chunk.len();
        raw.write_all(&chunk)?;
        for item in splitter.push(&chunk) {
            match item {
                StreamItem::Vt(b) => println!("vt {} bytes", b.len()),
                StreamItem::Rex(m) => println!("rex {m:?}"),
            }
        }
    }
    println!("stream bytes: {total}");
    Ok(())
}
