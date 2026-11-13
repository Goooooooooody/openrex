//! Checks the input the client sends against a running Rex server.
//!
//! These need a server on the local socket (or `REX_SERVER`), so they are
//! ignored by default:
//!
//!   cargo test -p openrex --test live_server -- --ignored
//!
//! Each test makes its own session and destroys it afterwards.

use std::time::{Duration, Instant};

use gpui::{Keystroke, Modifiers};
use openrex::keys::rex_key;
use openrex_proto::Endpoint;
use openrex_proto::control::{self, Control, Hello};
use serde_json::{Value, json};

struct Live {
    control: Control,
    session_id: String,
    block_id: String,
}

impl Live {
    async fn start(label: &str) -> Live {
        let endpoint = Endpoint::from_env().expect("endpoint");
        let hello = Hello { name: "openrex-test".into(), version: "0".into(), ..Default::default() };
        let conn = control::connect(&endpoint, hello).await.expect("connect to the Rex server");
        let control = conn.control.clone();
        smol::spawn(async move {
            if let Err(e) = conn.driver.await {
                eprintln!("control connection ended: {e:#}");
            }
        })
        .detach();
        let created = control
            .call(
                "session.create",
                json!({
                    "label": format!("openrex-test-{label}-{}", std::process::id()),
                    "initial_windows": [{"layout": {"block": {
                        "flavor": "com.superlogical.terminal.shell",
                        // A plain shell, so the user's prompt and startup files don't matter.
                        "options": {"shell": "none", "command": ["sh"]},
                    }}}],
                }),
            )
            .await
            .expect("session.create");
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let block_id = created["initial_windows"][0]["block_ids"][0].as_str().unwrap().to_string();
        control.call("session.attach", json!({"session_id": session_id})).await.expect("session.attach");
        Live { control, session_id, block_id }
    }

    async fn key(&self, keystroke: Keystroke) {
        let key = rex_key(&keystroke, false).expect("a key the server knows");
        self.control
            .call("client.key", json!({"session_id": self.session_id, "block_id": self.block_id, "key": key}))
            .await
            .expect("client.key");
    }

    /// Types text one key at a time, as the keyboard would.
    async fn type_text(&self, text: &str) {
        for c in text.chars() {
            let keystroke = match c {
                '\n' => Keystroke { modifiers: Modifiers::default(), key: "enter".into(), key_char: None },
                ' ' => Keystroke { modifiers: Modifiers::default(), key: "space".into(), key_char: Some(" ".into()) },
                c if c.is_ascii_uppercase() => Keystroke {
                    modifiers: Modifiers { shift: true, ..Default::default() },
                    key: c.to_ascii_lowercase().to_string(),
                    key_char: Some(c.to_string()),
                },
                c => Keystroke { modifiers: Modifiers::default(), key: c.to_string(), key_char: Some(c.to_string()) },
            };
            self.key(keystroke).await;
        }
    }

    async fn event(&self, kind: &str, payload: Value) {
        self.control
            .call(
                "block.event",
                json!({
                    "session_id": self.session_id,
                    "block_id": self.block_id,
                    "event": {"type": kind, "payload": payload},
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("block.event {kind}: {e}"));
    }

    async fn screen(&self) -> String {
        let v = self
            .control
            .block_call("com.superlogical.terminal.format", &self.session_id, &self.block_id, json!({}))
            .await
            .expect("format");
        v["content"].as_str().unwrap_or_default().to_string()
    }

    /// Waits until the screen contains `needle`.
    async fn expect(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = self.screen().await;
            if screen.contains(needle) {
                return;
            }
            if Instant::now() > deadline {
                panic!("the screen never showed {needle:?}. It shows:\n{screen}");
            }
            smol::Timer::after(Duration::from_millis(50)).await;
        }
    }

    async fn finish(self) {
        self.control.call("session.destroy", json!({"session_id": self.session_id})).await.ok();
        self.control.close();
    }
}

fn named(key: &str, modifiers: Modifiers) -> Keystroke {
    Keystroke { modifiers, key: key.into(), key_char: None }
}

#[test]
#[ignore = "needs a running Rex server"]
fn typed_keys_reach_the_shell() {
    smol::block_on(async {
        let live = Live::start("keys").await;
        // Letters, digits, shifted symbols, capitals, and enter.
        live.type_text("echo Typed-$((40+2))_OK\n").await;
        live.expect("Typed-42_OK").await;

        // Arrow keys and backspace edit the line.
        live.type_text("echo zqX").await;
        live.key(named("backspace", Modifiers::default())).await;
        live.key(named("left", Modifiers::default())).await;
        live.type_text("M\n").await;
        live.expect("zMq").await;

        // Ctrl+C interrupts a running command.
        live.type_text("sleep 300\n").await;
        smol::Timer::after(Duration::from_millis(300)).await;
        live.key(named("c", Modifiers { control: true, ..Default::default() })).await;
        live.type_text("echo after-$((1+1))\n").await;
        live.expect("after-2").await;
        live.finish().await;
    });
}

#[test]
#[ignore = "needs a running Rex server"]
fn paste_reaches_the_shell() {
    smol::block_on(async {
        let live = Live::start("paste").await;
        live.event("paste", json!({"text": "echo pasted-$((2+3))"})).await;
        live.type_text("\n").await;
        live.expect("pasted-5").await;
        live.finish().await;
    });
}

#[test]
#[ignore = "needs a running Rex server"]
fn mouse_and_wheel_reach_a_program_that_asks_for_them() {
    smol::block_on(async {
        let live = Live::start("mouse").await;
        // Turn on mouse reporting in SGR form, and show what arrives.
        live.type_text("printf '\\033[?1000h\\033[?1006h'; cat -v\n").await;
        smol::Timer::after(Duration::from_millis(400)).await;

        // The shapes the pane sends: a cell position counted from zero.
        live.event(
            "mouse",
            json!({"action": "press", "button": "left", "buttons": {"left": true}, "x": 2, "y": 1, "mods": ""}),
        )
        .await;
        live.expect("^[[<0;3;2M").await;
        live.event(
            "mouse",
            json!({"action": "release", "button": "left", "buttons": null, "x": 2, "y": 1, "mods": ""}),
        )
        .await;
        live.expect("^[[<0;3;2m").await;

        // A held modifier is added to the button: ctrl is 16.
        live.event(
            "mouse",
            json!({"action": "press", "button": "left", "buttons": {"left": true}, "x": 0, "y": 0, "mods": "ctrl"}),
        )
        .await;
        live.expect("^[[<16;1;1M").await;

        // Wheel up is a negative delta_y, and reports button 64.
        live.event("wheel", json!({"delta_mode": "line", "delta_x": 0, "delta_y": -1, "x": 2, "y": 1, "mods": ""}))
            .await;
        live.expect("^[[<64;").await;
        live.event("wheel", json!({"delta_mode": "line", "delta_x": 0, "delta_y": 1, "x": 2, "y": 1, "mods": ""}))
            .await;
        live.expect("^[[<65;").await;
        live.finish().await;
    });
}
