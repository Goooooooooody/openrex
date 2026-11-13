//! What the server can say about a terminal beyond its screen: its contents
//! as text or HTML (`format`), its processes, modes, size, and input mode,
//! and text it copied that was too large for an event (`clipboard_read`,
//! `blob_read`). Also the onboarding walkthrough blocks Rex can place in a
//! window, which OpenRex shows as a card.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{App, ClickEvent, Context, Div, InteractiveElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px, rgb};
use openrex_proto::control::{Control, RpcError};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target};
use crate::palette::{Item, Palette};
use crate::store::{Store, focused_block};
use crate::workspace::Workspace;

const FORMAT: &str = "com.superlogical.terminal.format";
const BLOB_READ: &str = "com.superlogical.terminal.blob_read";
const CLIPBOARD_READ: &str = "com.superlogical.terminal.clipboard_read";
const INPUT_MODE: &str = "com.superlogical.terminal.input_mode";
const ONBOARDING: &str = "com.superlogical.onboarding";

/// What the store keeps for this module.
#[derive(Default)]
pub struct InspectState {
    /// Terminals whose program has turned echo off, as while reading a
    /// password.
    pub echo_off: HashSet<String>,
    /// The step each onboarding block is on, once read.
    pub onboarding: HashMap<String, String>,
    onboarding_fetching: HashSet<String>,
}

/// Reads a result the server may split into parts: the first reply's
/// `content` or `data`, then `method` (`blob_read` or `clipboard_read`) from
/// where it left off until `eof`.
pub async fn read_all(
    control: &Control,
    session: &str,
    block: &str,
    first: Value,
    method: &str,
) -> Result<String, RpcError> {
    let mut text = first["content"].as_str().or(first["data"].as_str()).unwrap_or("").to_string();
    let mut id = first["id"].as_u64();
    let mut eof = first["eof"].as_bool().unwrap_or(id.is_none());
    // A generous cap, so a misbehaving server can't make this loop forever.
    let mut rounds = 0;
    while !eof && rounds < 10_000 {
        let Some(blob) = id else { break };
        rounds += 1;
        let part = control
            .block_call(method, session, block, json!({"id": blob, "offset": text.len(), "max_bytes": 983_040}))
            .await?;
        let data = part["data"].as_str().unwrap_or("");
        if data.is_empty() && !part["eof"].as_bool().unwrap_or(false) {
            break;
        }
        text.push_str(data);
        id = part["id"].as_u64().or(id);
        eof = part["eof"].as_bool().unwrap_or(true);
    }
    Ok(text)
}

/// The text a `clipboard_written` or `selection_copied` event left out
/// because it was too large: read it and put it on the clipboard.
pub fn fetch_copy(store: &mut Store, session: &str, block: &str, payload: &Value, method: &'static str, cx: &mut Context<Store>) {
    let Some(control) = store.control() else { return };
    let (session, block) = (session.to_string(), block.to_string());
    let mut args = json!({"max_bytes": 983_040});
    if let Some(id) = payload["id"].as_u64() {
        args["id"] = json!(id);
    }
    cx.spawn(async move |this, cx| {
        let text = match control.block_call(method, &session, &block, args).await {
            Ok(first) => read_all(&control, &session, &block, first, method).await,
            Err(e) => Err(e),
        };
        this.update(cx, |store, cx| {
            match text {
                Ok(text) if !text.is_empty() => store.pending_clipboard = Some(text),
                Ok(_) => {}
                Err(e) => store.error = Some(format!("reading copied text: {}", e.message)),
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// Handles the block events this module cares about.
pub fn on_block_event(store: &mut Store, session: &str, block: &str, name: &str, payload: &Value, cx: &mut Context<Store>) {
    match name {
        // Text a program copied, too large for the event.
        "clipboard_written" if payload["text"].as_str().is_none() && payload["bytes"].as_u64().unwrap_or(0) > 0 => {
            fetch_copy(store, session, block, payload, CLIPBOARD_READ, cx);
        }
        // Text another client selected and copied on the server's side.
        "selection_copied" => match payload["text"].as_str() {
            Some(text) => store.pending_clipboard = Some(text.to_string()),
            None if payload["id"].as_u64().is_some() => fetch_copy(store, session, block, payload, BLOB_READ, cx),
            None => {}
        },
        // A new foreground program may read a password.
        "process_changed" => check_echo(store, session, block, cx),
        "child_exited" => {
            store.inspect.echo_off.remove(block);
        }
        "step_changed" => {
            if let Some(step) = payload["step"].as_str() {
                store.inspect.onboarding.insert(block.to_string(), step.to_string());
            }
        }
        _ => {}
    }
}

/// Asks whether a terminal echoes what's typed, for the status bar's
/// password indicator.
pub fn check_echo(store: &mut Store, session: &str, block: &str, cx: &mut Context<Store>) {
    let Some(control) = store.control() else { return };
    let (session, block) = (session.to_string(), block.to_string());
    cx.spawn(async move |this, cx| {
        let result = control.block_call(INPUT_MODE, &session, &block, json!({})).await;
        this.update(cx, |store, cx| {
            // A server or system that can't tell (Windows) leaves it unknown,
            // which shows nothing.
            // A password prompt reads a line without echo. A full-screen
            // program also turns echo off, but reads keys raw, so only echo
            // off in line mode counts.
            let off = result.ok().is_some_and(|v| v["echo"].as_bool() == Some(false) && v["canonical"].as_bool() == Some(true));
            let changed = if off { store.inspect.echo_off.insert(block) } else { store.inspect.echo_off.remove(&block) };
            if changed {
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}

/// After Enter reaches a terminal, a program such as `sudo` or `read -s`
/// may start reading a password without a new foreground process. Look a
/// moment later.
pub fn after_enter(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(view) = ws.view(cx) else { return };
    let (Some(session), Some(block)) = (view.session_id.clone(), focused_block(&view).map(str::to_string)) else { return };
    let store = ws.store.clone();
    cx.spawn(async move |_, cx| {
        cx.background_executor().timer(Duration::from_millis(250)).await;
        store.update(cx, |s, cx| check_echo(s, &session, &block, cx)).ok();
    })
    .detach();
}

/// Whether the focused terminal is reading a password, for the status bar.
pub fn focused_echo_off(ws: &Workspace, cx: &App) -> bool {
    let store = ws.store.read(cx);
    store.current_view().and_then(focused_block).is_some_and(|b| store.inspect.echo_off.contains(b))
}

pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    let format = match action {
        "pane.info_copy" => return perform_copy(ws, action, args, cx),
        "pane.copy_text" => "text",
        "pane.copy_html" => "html",
        "pane.save_scrollback" => "text",
        "pane.info" => return Some(open_info(ws, target, cx)),
        _ => return None,
    };
    let Some((session, block)) = locate(ws, target, cx) else {
        return Some(Err(("conflict".into(), "no pane is focused".into())));
    };
    let Some(control) = ws.store.read(cx).control() else { return Some(Ok(Value::Null)) };
    let action = action.to_string();
    let path = args.get("path").and_then(Value::as_str).map(str::to_string);
    let label = ws.store.read(cx).panes.get(&block).map(|p| p.read(cx).title.clone()).unwrap_or_default();
    let store = ws.store.clone();
    cx.spawn(async move |this, cx| {
        let args = json!({"format": format, "trim": true, "unwrap": format == "text", "max_bytes": 983_040});
        let text = match control.block_call(FORMAT, &session, &block, args).await {
            Ok(first) => read_all(&control, &session, &block, first, BLOB_READ).await,
            Err(e) => Err(e),
        };
        let text = match text {
            Ok(t) => t,
            Err(e) => {
                store.update(cx, |s, cx| {
                    s.error = Some(format!("{action}: {}", e.message));
                    cx.notify();
                })
                .ok();
                return;
            }
        };
        if action == "pane.save_scrollback" {
            let path = path.map(std::path::PathBuf::from).unwrap_or_else(|| save_path(&label));
            let result = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::write(&path, &text));
            store
                .update(cx, |s, cx| {
                    match result {
                        Ok(()) => s.notice = Some(format!("Saved the scrollback to {}", path.display())),
                        Err(e) => s.error = Some(format!("saving {}: {e}", path.display())),
                    }
                    cx.notify();
                })
                .ok();
        } else {
            let what = if format == "html" { "as HTML" } else { "as text" };
            let lines = text.lines().count();
            this.update(cx, |ws, cx| ws.copy_text(text, &format!("Copied {lines} lines {what}"), cx)).ok();
        }
    })
    .detach();
    Some(Ok(Value::Null))
}

/// The session and block an action applies to.
fn locate(ws: &Workspace, target: &Target, cx: &App) -> Option<(String, String)> {
    let view = ws.view(cx)?;
    let session = target.session_id.clone().or(view.session_id.clone())?;
    let block = target.block_id.clone().or_else(|| focused_block(&view).map(str::to_string))?;
    Some((session, block))
}

/// `~/Downloads/<title or "scrollback">-<seconds since 1970>.txt`.
fn save_path(title: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    home.join("Downloads").join(format!("{}-{stamp}.txt", file_safe(title)))
}

/// A title made safe for a file name, or `scrollback` if nothing is left.
fn file_safe(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let trimmed: String = cleaned.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    // No leading dot, which would hide the file, and nothing made only of
    // dots and dashes, such as "..".
    let short: String = trimmed.trim_start_matches(['.', '-']).chars().take(60).collect();
    if short.chars().all(|c| c == '.' || c == '-') { "scrollback".into() } else { short }
}

/// A process as people read it: the script, else argv0, else the name,
/// with its ID and, when it isn't you, its user.
fn describe_process(p: &Value) -> Option<String> {
    let name = p["script"].as_str().or(p["argv0"].as_str()).or(p["name"].as_str())?;
    let mut out = format!("{name} (pid {})", p["pid"].as_u64().unwrap_or(0));
    if let Some(user) = p["user"].as_str() {
        if std::env::var("USER").ok().as_deref() != Some(user) {
            out.push_str(&format!(" as {user}"));
        }
    }
    Some(out)
}

/// The lines of the Pane Info palette, from the results of `process`,
/// `size`, `modes`, and `input_mode`.
fn info_lines(title: &str, cwd: Option<&str>, process: &Value, size: &Value, modes: &Value, input: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if !title.is_empty() {
        out.push(("Title".into(), title.to_string()));
    }
    if let Some(cwd) = cwd {
        out.push(("Directory".into(), cwd.to_string()));
    }
    if let Some(fg) = describe_process(&process["foreground"]) {
        out.push(("Running".into(), fg));
    }
    if let Some(child) = describe_process(&process["child"]) {
        out.push(("Started".into(), child));
    }
    if let Some(cwd) = process["foreground"]["cwd"].as_str().filter(|c| Some(*c) != cwd) {
        out.push(("Process directory".into(), cwd.to_string()));
    }
    let exit = &process["last_exit"];
    if !exit.is_null() {
        let code = exit["exit_code"].as_i64().unwrap_or(-1);
        let why = exit["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
        out.push(("Exited".into(), format!("code {code}{why}")));
    }
    if let (Some(c), Some(r)) = (size["columns"].as_u64(), size["rows"].as_u64()) {
        let owner = size["owner"].as_str().map(|o| format!(", owned by {}", short_id(o))).unwrap_or_else(|| ", no owner".into());
        out.push(("Size".into(), format!("{c}×{r}{owner}")));
    }
    if modes["screen"].as_str() == Some("alternate") {
        out.push(("Screen".into(), "alternate (a full-screen program)".into()));
    }
    let changed: Vec<String> = modes["modes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["set"].as_bool().unwrap_or(false) != m["default"].as_bool().unwrap_or(false))
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            Some(if m["set"].as_bool().unwrap_or(false) { name.to_string() } else { format!("no {name}") })
        })
        .collect();
    if !changed.is_empty() {
        out.push(("Modes".into(), changed.join(", ")));
    }
    if let Some(echo) = input["echo"].as_bool() {
        let mut parts = vec![if echo { "echo" } else { "no echo (reading a password?)" }];
        if input["canonical"].as_bool() == Some(false) {
            parts.push("raw");
        }
        if input["signals"].as_bool() == Some(false) {
            parts.push("no signal keys");
        }
        out.push(("Input".into(), parts.join(", ")));
    }
    out
}

fn short_id(id: &str) -> String {
    let tail: String = id.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    tail
}

/// Pane Info: what the focused terminal is running and how it's set up.
/// Choosing a line copies its value.
fn open_info(ws: &mut Workspace, target: &Target, cx: &mut Context<Workspace>) -> Outcome {
    let Some((session, block)) = locate(ws, target, cx) else { return Err(("conflict".into(), "no pane is focused".into())) };
    let Some(control) = ws.store.read(cx).control() else { return Ok(Value::Null) };
    let (title, cwd) = ws
        .store
        .read(cx)
        .panes
        .get(&block)
        .map(|p| (p.read(cx).title.clone(), p.read(cx).cwd.clone()))
        .unwrap_or_default();
    ws.palette = Some(Palette::pick("Pane Info", Vec::new()).empty("Asking the server…"));
    cx.spawn(async move |this, cx| {
        let call = |m: &'static str| control.block_call(m, &session, &block, json!({}));
        let process = call("com.superlogical.terminal.process").await.unwrap_or(Value::Null);
        let size = call("com.superlogical.terminal.size").await.unwrap_or(Value::Null);
        let modes = call("com.superlogical.terminal.modes").await.unwrap_or(Value::Null);
        let input = call(INPUT_MODE).await.unwrap_or(Value::Null);
        let mut lines = info_lines(&title, cwd.as_deref(), &process, &size, &modes, &input);
        lines.push(("Block".into(), block.clone()));
        this.update(cx, |ws, cx| {
            if let Some(p) = &mut ws.palette {
                if p.title == "Pane Info" {
                    p.items = lines
                        .into_iter()
                        .map(|(k, v)| Item::new(k, "pane.info_copy", json!({"text": v})).detail(v))
                        .collect();
                    p.empty = "The server said nothing about this pane".into();
                }
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// Copies one line of Pane Info.
pub(crate) fn perform_copy(ws: &mut Workspace, action: &str, args: &Value, cx: &mut Context<Workspace>) -> Option<Outcome> {
    if action != "pane.info_copy" {
        return None;
    }
    let text = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    ws.copy_text(text, "Copied", cx);
    Some(Ok(Value::Null))
}

/// A card for a block OpenRex can't draw as a terminal: Rex's onboarding
/// walkthrough, or a block of a kind it doesn't know.
pub fn block_card(ws: &Workspace, session: &str, block: &openrex_proto::types::Block, cx: &mut Context<Workspace>) -> Div {
    let Some(block_id) = block.block_id.clone() else { return div() };
    let onboarding = block.creator_name == ONBOARDING;
    if onboarding {
        fetch_step(ws, session, &block_id, cx);
    }
    let step = ws.store.read(cx).inspect.onboarding.get(&block_id).cloned();
    let (title, body) = if onboarding {
        let step = match step.as_deref() {
            Some("") | None => "the first step".to_string(),
            Some(s) => format!("step “{s}”"),
        };
        ("Rex walkthrough".to_string(), format!("Rex's onboarding walkthrough is on {step}. It is drawn by Rex.app; OpenRex can restart it or close it."))
    } else {
        (block.creator_name.clone(), "OpenRex doesn't know how to show this kind of block.".to_string())
    };
    let button = |id: &'static str, label: &'static str| {
        div().id(id).px_3().py_1().border_1().border_color(rgb(0x2a2c3d)).rounded_md().cursor_pointer().hover(|s| s.bg(rgb(0x24263a))).child(label)
    };
    let (s1, b1, s2, b2) = (session.to_string(), block_id.clone(), session.to_string(), block_id.clone());
    let mut buttons = div().flex().gap_2();
    if onboarding {
        buttons = buttons.child(button("onboarding-restart", "Start over").on_click(cx.listener(move |ws, _: &ClickEvent, _, cx| {
            let args = json!({"session_id": s1, "block_id": b1, "args": {"step": ""}});
            ws.store.update(cx, |s, cx| s.request("com.superlogical.onboarding.set_step", args, cx));
        })));
    }
    buttons = buttons.child(button("block-close", "Close").on_click(cx.listener(move |ws, _: &ClickEvent, _, cx| {
        ws.store.update(cx, |s, cx| s.request("block.close", json!({"session_id": s2, "block_id": b2}), cx));
    })));
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .p_4()
        .child(div().text_size(px(16.0)).child(SharedString::from(title)))
        .child(div().max_w(px(460.0)).text_color(rgb(0x6b7394)).child(SharedString::from(body)))
        .child(buttons)
}

fn fetch_step(ws: &Workspace, session: &str, block: &str, cx: &mut Context<Workspace>) {
    let store = ws.store.clone();
    let known = store.read(cx).inspect.onboarding.contains_key(block);
    if known || !store.update(cx, |s, _| s.inspect.onboarding_fetching.insert(block.to_string())) {
        return;
    }
    let Some(control) = store.read(cx).control() else { return };
    let (session, block) = (session.to_string(), block.to_string());
    cx.spawn(async move |_, cx| {
        let state = control.block_call("com.superlogical.onboarding.state", &session, &block, json!({})).await;
        store
            .update(cx, |s, cx| {
                s.inspect.onboarding_fetching.remove(&block);
                let step = state.ok().and_then(|v| v["step"].as_str().map(str::to_string)).unwrap_or_default();
                s.inspect.onboarding.insert(block, step);
                cx.notify();
            })
            .ok();
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_titles_safe_for_file_names() {
        assert_eq!(file_safe("goody@host: ~/src"), "goody-host-src");
        assert_eq!(file_safe(""), "scrollback");
        assert_eq!(file_safe("../.."), "scrollback");
        assert_eq!(file_safe("vim main.rs"), "vim-main.rs");
    }

    #[test]
    fn describes_a_pane_from_the_servers_answers() {
        let process = json!({
            "foreground": {"pid": 42, "name": "node", "script": "/srv/app.js", "user": "root", "user_id": 0, "cwd": "/srv"},
            "child": {"pid": 7, "name": "bash", "argv0": "-bash", "user_id": 1000},
            "last_exit": null
        });
        let size = json!({"columns": 120, "rows": 40, "owner": "client:01a1-abcdef", "desired": []});
        let modes = json!({"screen": "alternate", "modes": [
            {"name": "bracketed_paste", "number": 2004, "set": true},
            {"name": "cursor_visible", "number": 25, "set": false, "default": true},
            {"name": "wraparound", "number": 7, "set": true, "default": true}
        ]});
        let input = json!({"echo": false, "canonical": true, "signals": true});
        let lines = info_lines("app", Some("/home/me"), &process, &size, &modes, &input);
        let get = |k: &str| lines.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Running"), Some("/srv/app.js (pid 42) as root"));
        assert_eq!(get("Started"), Some("-bash (pid 7)"));
        assert_eq!(get("Process directory"), Some("/srv"));
        assert_eq!(get("Size"), Some("120×40, owned by abcdef"));
        assert_eq!(get("Modes"), Some("bracketed_paste, no cursor_visible"));
        assert_eq!(get("Input"), Some("no echo (reading a password?)"));
        assert!(get("Screen").is_some());
        assert_eq!(get("Exited"), None);
    }
}
