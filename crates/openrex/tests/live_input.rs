//! Checks that text an input method commits types correctly on a running
//! Rex server. Ignored by default, like live_server.rs:
//!
//!   cargo test -p openrex --test live_input -- --ignored
//!
//! The test makes its own session and destroys it afterwards.

use std::time::{Duration, Instant};

use openrex::input::text_key;
use openrex_proto::control::{self, Hello};
use openrex_proto::Endpoint;
use serde_json::json;

#[test]
#[ignore = "needs a running Rex server"]
fn committed_text_types_as_typed_not_pasted() {
    smol::block_on(async {
        let endpoint = Endpoint::from_env().expect("endpoint");
        let hello = Hello { name: "openrex-test".into(), version: "0".into(), ..Default::default() };
        let conn = control::connect(&endpoint, hello).await.expect("connect to the Rex server");
        let control = conn.control.clone();
        smol::spawn(async move {
            conn.driver.await.ok();
        })
        .detach();
        // Bracketed paste on, so text sent as a paste would show its brackets.
        let created = control
            .call(
                "session.create",
                json!({
                    "label": format!("openrex-test-input-{}", std::process::id()),
                    "initial_windows": [{"layout": {"block": {
                        "flavor": "com.superlogical.terminal.shell",
                        "options": {"shell": "none", "command": ["sh", "-c", "printf '\\033[?2004h'; stty raw -echo; cat -v"]},
                    }}}],
                }),
            )
            .await
            .expect("session.create");
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let block_id = created["initial_windows"][0]["block_ids"][0].as_str().unwrap().to_string();
        control.call("session.attach", json!({"session_id": session_id})).await.expect("session.attach");
        smol::Timer::after(Duration::from_millis(400)).await;

        for text in ["日本語", "é", "ok"] {
            control
                .call("client.key", json!({"session_id": session_id, "block_id": block_id, "key": text_key(text)}))
                .await
                .expect("client.key");
        }
        // cat -v shows each byte of UTF-8 above 127 as M-x.
        let want = "M-fM-^WM-%M-fM-^\\M-,M-hM-*M-^^M-CM-)ok";
        let deadline = Instant::now() + Duration::from_secs(5);
        let screen = loop {
            let v = control
                .block_call("com.superlogical.terminal.format", &session_id, &block_id, json!({}))
                .await
                .expect("format");
            let screen = v["content"].as_str().unwrap_or_default().to_string();
            if screen.contains(want) || Instant::now() > deadline {
                break screen;
            }
            smol::Timer::after(Duration::from_millis(50)).await;
        };
        control.call("session.destroy", json!({"session_id": session_id})).await.ok();
        control.close();
        assert!(screen.contains(want), "the screen shows:\n{screen}");
        assert!(!screen.contains("200~"), "the text was bracketed as a paste:\n{screen}");
    });
}
