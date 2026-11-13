//! What OpenRex's actions do. Key bindings, the command palette, and other
//! clients (through `client.perform`, as `rex do` sends) all come here.

use gpui::{ClipboardItem, Context, Window};
use openrex_proto::types::{LayoutNode, SessionView, Window as RexWindow};
use serde_json::{Value, json};

use crate::keymap::{self, ActionInfo};
use crate::palette::{Item, Palette, Suggest};
use crate::store::{active_window, focused_block};
use crate::workspace::Workspace;

/// Why an action wasn't performed: an error code the server knows, such as
/// `not_found` or `bad_request`, and a message for people.
pub type Failure = (String, String);

pub type Outcome = Result<Value, Failure>;

fn fail(code: &str, message: impl Into<String>) -> Outcome {
    Err((code.to_string(), message.into()))
}

fn done() -> Outcome {
    Ok(Value::Null)
}

/// Where an action applies, when the request names a session or block.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub session_id: Option<String>,
    pub block_id: Option<String>,
    pub origin: String,
    /// For `client.key.forward`: the key that performed it, and the keys of
    /// the sequence before it.
    pub key: Option<Value>,
    pub prefix: Vec<String>,
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A direction's split: `horizontal` and `before` for left, and so on.
fn split_of(direction: &str) -> Option<(&'static str, &'static str)> {
    Some(match direction {
        "left" => ("horizontal", "before"),
        "right" => ("horizontal", "after"),
        "up" => ("vertical", "before"),
        "down" => ("vertical", "after"),
        _ => return None,
    })
}

/// Every placed block of a window's tiled layout with its rectangle.
pub fn tiled_leaves(win: &RexWindow) -> Vec<(String, [f32; 4])> {
    let mut out = Vec::new();
    for layer in win.layers.iter().filter(|l| !l.is_floating()) {
        if let Some(layout) = &layer.layout {
            layout.leaves(&mut out);
        }
    }
    out
}

/// The nearest block to `from` in `direction`, by the layout's geometry.
pub fn neighbor(win: &RexWindow, from: &str, direction: &str) -> Option<String> {
    let leaves = tiled_leaves(win);
    let (_, r) = leaves.iter().find(|(id, _)| id == from)?;
    let (cx, cy) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
    const E: f32 = 0.001;
    leaves
        .iter()
        .filter(|(id, _)| id != from)
        .filter(|(_, o)| {
            let overlap_y = o[1] < r[1] + r[3] - E && o[1] + o[3] > r[1] + E;
            let overlap_x = o[0] < r[0] + r[2] - E && o[0] + o[2] > r[0] + E;
            match direction {
                "left" => overlap_y && o[0] + o[2] <= r[0] + E,
                "right" => overlap_y && o[0] >= r[0] + r[2] - E,
                "up" => overlap_x && o[1] + o[3] <= r[1] + E,
                "down" => overlap_x && o[1] >= r[1] + r[3] - E,
                _ => false,
            }
        })
        .min_by(|(_, a), (_, b)| {
            let d = |o: &[f32; 4]| (o[0] + o[2] / 2.0 - cx).powi(2) + (o[1] + o[3] / 2.0 - cy).powi(2);
            d(a).total_cmp(&d(b))
        })
        .map(|(id, _)| id.clone())
}

/// The split ratios that give every pane of a layout an equal share along
/// each split's direction.
pub fn balanced_ratios(node: &LayoutNode, out: &mut Vec<(String, String, String, f32)>) {
    let Some(split) = &node.split else { return };
    let count = |n: &LayoutNode| along(n, &split.direction);
    let (b, a) = (count(&split.before), count(&split.after));
    let mut before = Vec::new();
    split.before.leaves(&mut before);
    let mut after = Vec::new();
    split.after.leaves(&mut after);
    if let (Some((bid, _)), Some((aid, _))) = (before.last(), after.first()) {
        out.push((split.direction.clone(), bid.clone(), aid.clone(), b as f32 / (a + b) as f32));
    }
    balanced_ratios(&split.before, out);
    balanced_ratios(&split.after, out);
}

/// How many panes sit side by side along `direction` in a node.
fn along(node: &LayoutNode, direction: &str) -> usize {
    match &node.split {
        None => 1,
        Some(s) if s.direction == direction => along(&s.before, direction) + along(&s.after, direction),
        Some(s) => along(&s.before, direction).max(along(&s.after, direction)),
    }
}

impl Workspace {
    /// Whether OpenRex or the server can perform an action, for choosing
    /// between bindings on one key.
    pub fn can_perform(&self, action: &str, cx: &gpui::App) -> bool {
        action == "client.key.forward"
            || self.client_actions.iter().any(|a| a.name == action)
            || self.store.read(cx).keymap.server_actions.iter().any(|a| a.name == action)
            || action.starts_with("com.superlogical.")
    }

    /// Performs an action by name.
    pub fn perform(&mut self, action: &str, args: &Value, target: Target, window: &mut Window, cx: &mut Context<Self>) -> Outcome {
        let args = if args.is_null() { &json!({}) } else { args };
        let dir_action = |prefix: &str| action.strip_prefix(prefix).filter(|d| split_of(d).is_some() || *d == "auto");
        if let Some(d) = dir_action("pane.split.") {
            return self.split_pane(d, cx);
        }
        if let Some(d) = dir_action("pane.focus.") {
            return self.session(cx, "session.focus_direction", json!({"direction": d}));
        }
        if let Some(d) = dir_action("pane.move.") {
            return self.move_pane(d, cx);
        }
        match action {
            // App
            "client.palette.toggle" => {
                if self.palette.is_some() {
                    self.palette = None;
                } else {
                    self.open_commands(cx);
                }
                done()
            }
            "client.quit" => {
                cx.quit();
                done()
            }
            "client.reconnect" => {
                self.store.update(cx, |s, cx| s.connect(cx));
                done()
            }
            "client.notice.dismiss" => {
                self.store.update(cx, |s, cx| {
                    s.error = None;
                    s.notice = None;
                    cx.notify();
                });
                done()
            }
            "client.config.copy_path" => {
                let path = self.store.read(cx).keymap.config_path.clone();
                if path.is_empty() {
                    return fail("not_found", "the server hasn't said where its configuration file is");
                }
                self.copy_text(path.clone(), "Copied the config file path", cx);
                Ok(json!({"path": path}))
            }
            "client.config.reload" => {
                self.store.update(cx, |s, cx| {
                    s.call("server.config_reload", json!({}), cx, |s, r, cx| match r {
                        Ok(_) => {
                            s.notice = Some("Reloaded the configuration file".into());
                            s.fetch_keymap(cx);
                        }
                        Err(e) => s.error = Some(format!("server.config_reload: {}", e.message)),
                    })
                });
                done()
            }
            "client.close" => {
                let has_pane = self.store.read(cx).focused_pane().is_some();
                if has_pane {
                    self.store.update(cx, |s, cx| s.close_focused_pane(cx));
                } else if let Some(win) = self.active_window_id(cx) {
                    self.store.update(cx, |s, cx| s.close_window(win, cx));
                }
                done()
            }

            // Session
            "session.new" => {
                self.store.update(cx, |s, cx| s.new_session(cx));
                done()
            }
            "session.switch" => {
                self.open_sessions(cx);
                done()
            }
            "session.select" => {
                let Some(want) = arg_str(args, "session") else { return fail("bad_request", "session is required") };
                let id = {
                    let store = self.store.read(cx);
                    store
                        .sessions
                        .iter()
                        .find(|s| s.id == want || s.label == want || s.id.ends_with(want))
                        .map(|s| s.id.clone())
                };
                match id {
                    Some(id) => {
                        self.store.update(cx, |s, cx| s.select_session(id, cx));
                        done()
                    }
                    None => fail("not_found", format!("no session matches {want}")),
                }
            }
            "session.next" | "session.previous" => {
                let next = {
                    let store = self.store.read(cx);
                    let n = store.sessions.len();
                    let i = store.sessions.iter().position(|s| Some(&s.id) == store.current.as_ref());
                    match (i, n) {
                        (_, 0) => None,
                        (None, _) => store.sessions.first().map(|s| s.id.clone()),
                        (Some(i), n) => {
                            let j = if action == "session.next" { (i + 1) % n } else { (i + n - 1) % n };
                            Some(store.sessions[j].id.clone())
                        }
                    }
                };
                if let Some(id) = next {
                    self.store.update(cx, |s, cx| s.select_session(id, cx));
                }
                done()
            }
            "session.rename" => {
                let session = arg_str(args, "session").map(str::to_string).or_else(|| self.store.read(cx).current.clone());
                let Some(session) = session else { return fail("conflict", "no session is open") };
                match args.get("label").and_then(Value::as_str) {
                    Some(label) => {
                        self.store.update(cx, |s, cx| s.request("session.set_label", json!({"session_id": session, "label": label}), cx));
                    }
                    None => {
                        let current = self.session_label(&session, cx);
                        self.palette = Some(
                            Palette::input("Rename Session", "session.rename", json!({"session": session}), "label", &current)
                                .placeholder("A name for the session"),
                        );
                    }
                }
                done()
            }
            "session.close" => {
                let session = arg_str(args, "session").map(str::to_string).or_else(|| self.store.read(cx).current.clone());
                let Some(session) = session else { return fail("conflict", "no session is open") };
                // Ending a session ends its programs, so a key asks first.
                if args.get("confirm").and_then(Value::as_bool) == Some(true) || target.origin == "api" || target.origin == "cli" {
                    self.store.update(cx, |s, cx| s.destroy_session(session, cx));
                } else {
                    let label = self.session_label(&session, cx);
                    self.palette = Some(Palette::pick(
                        format!("Close session {label} and end its programs?"),
                        vec![
                            Item::new("Close Session", "session.close", json!({"session": session, "confirm": true})),
                            Item::new("Cancel", "client.palette.close", Value::Null),
                        ],
                    ));
                }
                done()
            }
            "client.palette.close" => {
                self.palette = None;
                done()
            }
            "session.refresh" => {
                self.store.update(cx, |s, cx| s.refresh_all(cx));
                done()
            }
            "session.move.up" | "session.move.down" => {
                let (session, before) = {
                    let store = self.store.read(cx);
                    let Some(cur) = store.current.clone() else { return fail("conflict", "no session is open") };
                    let i = store.sessions.iter().position(|s| s.id == cur).unwrap_or(0);
                    let before = if action == "session.move.up" {
                        if i == 0 { return done() }
                        Some(store.sessions[i - 1].id.clone())
                    } else {
                        store.sessions.get(i + 2).map(|s| s.id.clone())
                    };
                    if action == "session.move.down" && i + 1 >= store.sessions.len() {
                        return done();
                    }
                    (cur, before)
                };
                let mut payload = json!({"session_id": session});
                if let Some(before) = before {
                    payload["before_session_id"] = json!(before);
                }
                self.store.update(cx, |s, cx| s.request("session.move", payload, cx));
                done()
            }

            // Tab
            "client.tab.new" => {
                self.store.update(cx, |s, cx| s.new_window(cx));
                done()
            }
            "client.tab.close" => {
                let Some(win) = arg_str(args, "window").map(str::to_string).or_else(|| self.active_window_id(cx)) else {
                    return fail("conflict", "no tab is open");
                };
                self.store.update(cx, |s, cx| s.close_window(win, cx));
                done()
            }
            "client.tab.close_others" => {
                let keep = self.active_window_id(cx);
                let others: Vec<String> = self
                    .view(cx)
                    .map(|v| v.windows.iter().filter_map(|w| w.window_id.clone()).filter(|id| Some(id) != keep.as_ref()).collect())
                    .unwrap_or_default();
                self.store.update(cx, |s, cx| others.into_iter().for_each(|w| s.close_window(w, cx)));
                done()
            }
            "client.tab.rename" => {
                let Some(win) = arg_str(args, "window").map(str::to_string).or_else(|| self.active_window_id(cx)) else {
                    return fail("conflict", "no tab is open");
                };
                match args.get("label").and_then(Value::as_str) {
                    Some(label) => self.session(cx, "session.set_window_label", json!({"window_id": win, "label": label})),
                    None => {
                        let current = self
                            .view(cx)
                            .and_then(|v| v.windows.into_iter().find(|w| w.window_id.as_deref() == Some(win.as_str())))
                            .map(|w| w.label)
                            .unwrap_or_default();
                        self.palette = Some(
                            Palette::input("Rename Tab", "client.tab.rename", json!({"window": win}), "label", &current)
                                .placeholder("A name for the tab. Empty restores the default."),
                        );
                        done()
                    }
                }
            }
            "client.tab.next" => {
                self.store.update(cx, |s, cx| s.cycle_window(true, cx));
                done()
            }
            "client.tab.previous" => {
                self.store.update(cx, |s, cx| s.cycle_window(false, cx));
                done()
            }
            "client.tab.goto" => {
                let index = args.get("index").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
                let id = self.view(cx).and_then(|v| v.windows.into_iter().nth(index - 1)).and_then(|w| w.window_id);
                match id {
                    Some(id) => {
                        self.store.update(cx, |s, cx| s.focus_window(id, cx));
                        done()
                    }
                    None => fail("not_found", format!("there is no tab {index}")),
                }
            }
            "client.tab.move" | "client.tab.move.backward" | "client.tab.move.forward" => {
                let Some(view) = self.view(cx) else { return fail("conflict", "no session is open") };
                let ids: Vec<String> = view.windows.iter().filter_map(|w| w.window_id.clone()).collect();
                let Some(active) = view.active_window_id.clone() else { return done() };
                let Some(from) = ids.iter().position(|id| *id == active) else { return done() };
                let to = match action {
                    "client.tab.move.backward" => from.saturating_sub(1),
                    "client.tab.move.forward" => (from + 1).min(ids.len() - 1),
                    _ => (args.get("index").and_then(Value::as_u64).unwrap_or(1).max(1) as usize - 1).min(ids.len() - 1),
                };
                if to == from {
                    return done();
                }
                // Placed before the tab now at `to`, or that tab's successor
                // when moving later.
                let before = if to > from { ids.get(to + 1) } else { ids.get(to) };
                let mut payload = json!({"window_id": active});
                if let Some(b) = before {
                    payload["before_window_id"] = json!(b);
                }
                self.session(cx, "session.move_window", payload)
            }

            // Pane
            "pane.split" => self.split_pane(arg_str(args, "direction").unwrap_or("right"), cx),
            "pane.focus" => self.session(cx, "session.focus_direction", json!({"direction": arg_str(args, "direction").unwrap_or("right")})),
            "pane.focus_block" => {
                let Some(block) = arg_str(args, "block") else { return fail("bad_request", "block is required") };
                self.session(cx, "session.focus_block", json!({"block_id": block}))
            }
            "pane.focus_next" => self.session(cx, "session.focus_next_pane", json!({})),
            "pane.focus_previous" => self.session(cx, "session.focus_previous_pane", json!({})),
            "pane.move" => self.move_pane(arg_str(args, "direction").unwrap_or("right"), cx),
            "pane.zoom" => {
                let block = self.target_block(&target, cx);
                let mut payload = json!({});
                if let Some(b) = block {
                    payload["block_id"] = json!(b);
                }
                self.session(cx, "session.zoom_block", payload)
            }
            "pane.close" => {
                let Some(block) = self.target_block(&target, cx) else { return fail("conflict", "no pane is focused") };
                self.session(cx, "block.close", json!({"block_id": block}))
            }
            "pane.close_others" => {
                let keep = self.target_block(&target, cx);
                let others: Vec<String> = self
                    .view(cx)
                    .and_then(|v| active_window(&v).cloned())
                    .map(|w| w.layers.iter().flat_map(|l| l.blocks.iter()).filter_map(|b| b.block_id.clone()).filter(|b| Some(b) != keep.as_ref()).collect())
                    .unwrap_or_default();
                for b in others {
                    self.session(cx, "block.close", json!({"block_id": b}))?;
                }
                done()
            }
            "pane.balance" => {
                let Some(win) = self.view(cx).and_then(|v| active_window(&v).cloned()) else { return done() };
                let mut ratios = Vec::new();
                for layer in &win.layers {
                    if let Some(layout) = &layer.layout {
                        balanced_ratios(layout, &mut ratios);
                    }
                }
                self.store.update(cx, |s, cx| {
                    for (direction, before, after, ratio) in ratios {
                        s.resize_split(&direction, &before, &after, ratio, cx);
                    }
                });
                done()
            }
            "pane.resize" => {
                let direction = arg_str(args, "direction").unwrap_or("right");
                let amount = args.get("amount").and_then(Value::as_f64).unwrap_or(5.0) as f32 / 100.0;
                self.resize_pane(direction, amount, cx)
            }
            "pane.float" => self.toggle_float(cx),
            "pane.new_floating" => self.session(
                cx,
                "session.new_layer",
                json!({"bounds": [0.15, 0.12, 0.7, 0.7], "layout": crate::store::shell_layout_in(self.focused_cwd(cx)), "focus": true}),
            ),
            "pane.minimize_floating" | "pane.minimize_all_floating" => self.minimize_floating(action == "pane.minimize_all_floating", cx),
            "pane.detach" => {
                let Some(block) = self.target_block(&target, cx) else { return fail("conflict", "no pane is focused") };
                self.session(cx, "session.detach_block", json!({"block_id": block}))
            }
            "pane.attach" => self.attach_detached(arg_str(args, "block"), cx),
            "pane.move_to_new_tab" => {
                let Some(block) = self.target_block(&target, cx) else { return fail("conflict", "no pane is focused") };
                self.move_to_new_tab(block, cx)
            }
            "pane.move_to_tab" => {
                let Some(block) = self.target_block(&target, cx) else { return fail("conflict", "no pane is focused") };
                match args.get("index").and_then(Value::as_u64) {
                    Some(index) => self.move_to_tab(block, index as usize, cx),
                    None => {
                        let items = self
                            .view(cx)
                            .map(|v| {
                                v.windows
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, w)| w.window_id != v.active_window_id)
                                    .map(|(i, w)| Item::new(self.window_title(w, cx), "pane.move_to_tab", json!({"index": i + 1})).detail(format!("tab {}", i + 1)))
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.palette = Some(Palette::pick("Move Pane to Tab", items).empty("There is no other tab"));
                        done()
                    }
                }
            }
            "pane.rename" => {
                let Some(block) = arg_str(args, "block").map(str::to_string).or_else(|| self.target_block(&target, cx)) else {
                    return fail("conflict", "no pane is focused");
                };
                match args.get("label").and_then(Value::as_str) {
                    Some(label) => self.session(cx, "session.set_block_label", json!({"block_id": block, "label": label})),
                    None => {
                        let current = self.block_label(&block, cx);
                        self.palette = Some(
                            Palette::input("Rename Pane", "pane.rename", json!({"block": block}), "label", &current)
                                .placeholder("A name for the pane. Empty restores the default."),
                        );
                        done()
                    }
                }
            }
            "pane.copy_id" => {
                let Some(block) = self.target_block(&target, cx) else { return fail("conflict", "no pane is focused") };
                self.copy_text(block.clone(), "Copied the pane's ID", cx);
                Ok(json!({"block_id": block}))
            }
            "pane.copy" => {
                if let Some(pane) = self.store.read(cx).focused_pane() {
                    pane.update(cx, |pane, cx| pane.copy_selection(cx));
                }
                done()
            }
            "pane.paste" => {
                let text = cx.read_from_clipboard().and_then(|item| item.text());
                if let (Some(text), Some(pane)) = (text, self.store.read(cx).focused_pane()) {
                    pane.update(cx, |pane, _| pane.paste(text));
                }
                done()
            }
            "pane.select_all" => {
                if let Some(pane) = self.store.read(cx).focused_pane() {
                    pane.update(cx, |pane, cx| pane.select_all(cx));
                }
                done()
            }
            "pane.send_key" => {
                let Some(spec) = arg_str(args, "key") else { return fail("bad_request", "key is required") };
                let Some(key) = keymap::parse_key(spec) else { return fail("bad_request", format!("can't read the key {spec}")) };
                let Some(block) = self.target_block(&target, cx) else { return fail("conflict", "no pane is focused") };
                self.session(cx, "block.event", json!({"block_id": block, "event": {"type": "key", "payload": keymap::key_event_of(&key)}}))
            }
            "client.key.forward" => {
                let (Some(key), Some(session)) = (target.key.clone(), self.store.read(cx).current.clone()) else {
                    return fail("bad_request", "only a key can perform client.key.forward");
                };
                let mut payload = json!({"session_id": session, "key": key});
                if !target.prefix.is_empty() {
                    payload["prefix"] = json!(target.prefix);
                }
                if let Some(control) = self.store.read(cx).control() {
                    control.send_nowait("client.key", payload);
                }
                done()
            }
            "pane.clear" | "pane.reset" => {
                let method = if action == "pane.clear" { "com.superlogical.terminal.clear" } else { "com.superlogical.terminal.reset" };
                self.server_action(method, &Value::Null, &target, cx);
                done()
            }
            "pane.go_to" => {
                self.open_go_to(cx);
                done()
            }
            "pane.go_to_directory" => match arg_str(args, "path") {
                Some(path) => {
                    let path = path.to_string();
                    self.store.update(cx, |s, cx| s.new_window_in(Some(path), cx));
                    done()
                }
                None => self.open_path_input("Go to Directory", "pane.go_to_directory", false, cx),
            },
            "pane.insert_path" => match arg_str(args, "path") {
                Some(path) => {
                    let quoted = shell_quote(path);
                    if let Some(pane) = self.store.read(cx).focused_pane() {
                        pane.update(cx, |pane, _| pane.paste(quoted));
                    }
                    done()
                }
                None => self.open_path_input("Insert Path", "pane.insert_path", true, cx),
            },
            "client.scroll.page_up" | "client.scroll.page_down" | "client.scroll.top" | "client.scroll.bottom" => {
                if let Some(pane) = self.store.read(cx).focused_pane() {
                    pane.update(cx, |pane, cx| {
                        let page = pane.grid_size().map_or(10, |(_, rows)| rows as isize - 1);
                        let lines = match action {
                            "client.scroll.page_up" => -page,
                            "client.scroll.page_down" => page,
                            "client.scroll.top" => isize::MIN / 2,
                            _ => isize::MAX / 2,
                        };
                        pane.scroll_lines(lines, cx);
                    });
                }
                done()
            }

            // Keys
            "client.mode.enter" => {
                let Some(name) = arg_str(args, "name") else { return fail("bad_request", "name is required") };
                if !self.modes.iter().any(|m| m == name) {
                    self.modes.push(name.to_string());
                }
                done()
            }
            "client.mode.exit" => {
                self.modes.pop();
                done()
            }
            "client.mode.exit_all" => {
                self.modes.clear();
                done()
            }

            // View
            "client.font.increase" | "client.font.decrease" | "client.font.reset" => {
                let delta = match action {
                    "client.font.increase" => Some(1.0),
                    "client.font.decrease" => Some(-1.0),
                    _ => None,
                };
                self.change_font_size(delta, cx);
                done()
            }
            "client.sidebar.toggle" => {
                self.sidebar_visible = !self.sidebar_visible;
                done()
            }

            // Anything else is the configuration file's or a block's.
            _ if self.store.read(cx).keymap.server_actions.iter().any(|a| a.name == action) || action.contains("com.superlogical.") => {
                self.server_action(action, args, &target, cx);
                done()
            }
            _ => match self.perform_extra(action, args, &target, window, cx) {
                Some(outcome) => outcome,
                None => fail("not_supported", format!("OpenRex has no action named {action}")),
            },
        }
    }

    /// Runs an action of the configuration file or a block on the server,
    /// then the client actions it asks for.
    pub fn server_action(&mut self, action: &str, args: &Value, target: &Target, cx: &mut Context<Self>) {
        let Some(session) = target.session_id.clone().or_else(|| self.store.read(cx).current.clone()) else { return };
        let mut payload = json!({
            "action": action,
            "origin": if target.origin.is_empty() { "api" } else { target.origin.as_str() },
            "session_id": session,
        });
        if args.as_object().is_some_and(|o| !o.is_empty()) {
            payload["args"] = args.clone();
        }
        if let Some(block) = &target.block_id {
            payload["block_id"] = json!(block);
        }
        let Some(control) = self.store.read(cx).control() else { return };
        let action = action.to_string();
        cx.spawn(async move |this, cx| {
            let result = control.call("client.action", payload).await;
            this.update(cx, |this, cx| match result {
                Ok(v) => {
                    let queued: Vec<(String, Value)> = v["client_actions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| Some((a["action"].as_str()?.to_string(), a.get("args").cloned().unwrap_or(Value::Null))))
                        .collect();
                    this.queued_actions.extend(queued);
                    cx.notify();
                }
                Err(e) => this.store.update(cx, |s, cx| {
                    s.error = Some(format!("{action}: {}", e.message));
                    cx.notify();
                }),
            })
            .ok();
        })
        .detach();
    }

    // Helpers.

    pub fn view(&self, cx: &gpui::App) -> Option<SessionView> {
        self.store.read(cx).current_view().cloned()
    }

    pub fn active_window_id(&self, cx: &gpui::App) -> Option<String> {
        self.view(cx).and_then(|v| v.active_window_id.clone())
    }

    fn target_block(&self, target: &Target, cx: &gpui::App) -> Option<String> {
        target.block_id.clone().or_else(|| self.view(cx).as_ref().and_then(focused_block).map(str::to_string))
    }

    /// Calls a session method for its effect, in the current session.
    pub fn session(&mut self, cx: &mut Context<Self>, method: &'static str, payload: Value) -> Outcome {
        if self.store.read(cx).current.is_none() {
            return fail("conflict", "no session is open");
        }
        self.store.update(cx, |s, cx| s.session_request(method, payload, cx));
        done()
    }

    pub fn copy_text(&mut self, text: String, notice: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let notice = notice.to_string();
        self.store.update(cx, |s, cx| {
            s.notice = Some(notice);
            cx.notify();
        });
    }

    fn session_label(&self, id: &str, cx: &gpui::App) -> String {
        self.store.read(cx).sessions.iter().find(|s| s.id == id).map(|s| s.label.clone()).unwrap_or_default()
    }

    fn block_label(&self, id: &str, cx: &gpui::App) -> String {
        self.view(cx)
            .into_iter()
            .flat_map(|v| v.windows)
            .flat_map(|w| w.layers)
            .flat_map(|l| l.blocks)
            .find(|b| b.block_id.as_deref() == Some(id))
            .map(|b| b.label)
            .unwrap_or_default()
    }

    /// The directory the focused terminal last reported, for new panes.
    pub fn focused_cwd(&self, cx: &gpui::App) -> Option<String> {
        let pane = self.store.read(cx).focused_pane()?;
        pane.read(cx).cwd.clone()
    }

    fn split_pane(&mut self, direction: &str, cx: &mut Context<Self>) -> Outcome {
        let direction = if direction == "auto" {
            // Split across the longer side, as the cell grid is taller than wide.
            match self.store.read(cx).focused_pane().and_then(|p| p.read(cx).grid_size()) {
                Some((cols, rows)) if (rows as f32) * 2.2 > cols as f32 => "down",
                _ => "right",
            }
        } else {
            direction
        };
        let Some((split, side)) = split_of(direction) else { return fail("bad_request", format!("unknown direction {direction}")) };
        let cwd = self.focused_cwd(cx);
        self.store.update(cx, |s, cx| s.split_side(split, side, cwd, cx));
        done()
    }

    fn move_pane(&mut self, direction: &str, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return fail("conflict", "no session is open") };
        let Some(win) = active_window(&view) else { return done() };
        let Some(from) = win.focused_block_id.clone() else { return done() };
        match neighbor(win, &from, direction) {
            Some(other) => self.session(cx, "session.swap_blocks", json!({"block_id": from, "other_block_id": other})),
            None => done(),
        }
    }

    fn resize_pane(&mut self, direction: &str, amount: f32, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return done() };
        let Some(win) = active_window(&view) else { return done() };
        let Some(focused) = win.focused_block_id.clone() else { return done() };
        let want = if matches!(direction, "left" | "right") { "horizontal" } else { "vertical" };
        // The innermost split along the direction that holds the focused pane.
        let mut found = None;
        for layer in win.layers.iter().filter(|l| !l.is_floating()) {
            let Some(layout) = &layer.layout else { continue };
            let mut splits = Vec::new();
            layout.splits(&mut splits);
            for (split, _) in splits {
                if split.direction != want {
                    continue;
                }
                let (mut before, mut after) = (Vec::new(), Vec::new());
                split.before.leaves(&mut before);
                split.after.leaves(&mut after);
                if before.iter().chain(&after).any(|(id, _)| *id == focused) {
                    if let (Some((b, _)), Some((a, _))) = (before.last(), after.first()) {
                        found = Some((b.clone(), a.clone(), split.ratio));
                    }
                }
            }
        }
        let Some((before, after, ratio)) = found else { return done() };
        // Left and up move the border toward the start.
        let ratio = if matches!(direction, "left" | "up") { ratio - amount } else { ratio + amount };
        self.store.update(cx, |s, cx| s.resize_split(want, &before, &after, ratio, cx));
        done()
    }

    fn toggle_float(&mut self, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return done() };
        let Some(win) = active_window(&view) else { return done() };
        let Some(block) = win.focused_block_id.clone() else { return done() };
        let floating = win.layers.iter().any(|l| l.is_floating() && l.blocks.iter().any(|b| b.block_id.as_deref() == Some(block.as_str())));
        if floating {
            // Dock it beside the first tiled pane.
            let anchor = tiled_leaves(win).into_iter().map(|(id, _)| id).find(|id| *id != block);
            match anchor {
                Some(anchor) => self.session(cx, "session.move_block", json!({"block_id": block, "anchor_block_id": anchor, "direction": "horizontal", "side": "after"})),
                None => fail("conflict", "there is no tiled pane to dock beside"),
            }
        } else {
            self.session(cx, "session.float_block", json!({"block_id": block, "bounds": [0.15, 0.12, 0.7, 0.7]}))
        }
    }

    fn minimize_floating(&mut self, all: bool, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return done() };
        let Some(win) = active_window(&view) else { return done() };
        let focused = win.focused_block_id.clone();
        let floating: Vec<_> = win.layers.iter().filter(|l| l.is_floating()).collect();
        if floating.is_empty() {
            return done();
        }
        if all {
            // Minimize every one, or restore them all when all are minimized.
            let restore = floating.iter().all(|l| l.is_minimized());
            for l in floating {
                if let Some(id) = &l.layer_id {
                    self.session(cx, "session.set_layer_state", json!({"layer_id": id, "state": if restore { "normal" } else { "minimized" }}))?;
                }
            }
            return done();
        }
        let holds_focus = |l: &&openrex_proto::types::Layer| l.blocks.iter().any(|b| b.block_id == focused);
        let layer = floating.iter().find(|l| holds_focus(l) && !l.is_minimized()).or_else(|| floating.iter().rev().find(|l| l.is_minimized()));
        let Some(layer) = layer else { return done() };
        let Some(id) = layer.layer_id.clone() else { return done() };
        if layer.is_minimized() {
            self.session(cx, "session.set_layer_state", json!({"layer_id": id, "state": "normal", "focus": true}))
        } else {
            self.session(cx, "session.set_layer_state", json!({"layer_id": id, "state": "minimized"}))
        }
    }

    fn attach_detached(&mut self, block: Option<&str>, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return fail("conflict", "no session is open") };
        let session = view.session_id.clone().unwrap_or_default();
        match block {
            Some(block) => {
                let anchor = active_window(&view).and_then(|w| w.focused_block_id.clone().or_else(|| tiled_leaves(w).first().map(|(id, _)| id.clone())));
                match anchor {
                    Some(anchor) => self.session(cx, "session.move_block", json!({"block_id": block, "anchor_block_id": anchor, "direction": "horizontal", "side": "after"})),
                    None => self.session(cx, "session.focus_block", json!({"block_id": block})),
                }
            }
            None => {
                self.palette = Some(Palette::pick("Detached Panes", Vec::new()).empty("Looking for detached panes…"));
                let Some(control) = self.store.read(cx).control() else { return done() };
                cx.spawn(async move |this, cx| {
                    let result = control.call("session.list_blocks", json!({"session_id": session})).await;
                    this.update(cx, |this, cx| {
                        let items: Vec<Item> = result
                            .ok()
                            .and_then(|v| v["blocks"].as_array().cloned())
                            .into_iter()
                            .flatten()
                            .filter(|b| b["placement"].as_str() == Some("detached"))
                            .filter_map(|b| {
                                let id = b["block_id"].as_str()?.to_string();
                                let label = b["label"].as_str().unwrap_or("").to_string();
                                Some(Item::new(if label.is_empty() { id.clone() } else { label }, "pane.attach", json!({"block": id})).detail("detached"))
                            })
                            .collect();
                        if let Some(p) = &mut this.palette {
                            if p.title == "Detached Panes" {
                                p.items = items;
                                p.empty = "There are no detached panes".into();
                            }
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
                done()
            }
        }
    }

    fn move_to_new_tab(&mut self, block: String, cx: &mut Context<Self>) -> Outcome {
        // Make a tab with a placeholder shell, move the pane beside it, and
        // close the placeholder.
        let Some(control) = self.store.read(cx).control() else { return done() };
        let Some(session) = self.store.read(cx).current.clone() else { return done() };
        cx.spawn(async move |this, cx| {
            let made = control
                .call("session.new_window", json!({"session_id": session, "layout": crate::store::shell_layout_in(None)}))
                .await;
            let placeholder = made.ok().and_then(|v| v["block_ids"].as_array().and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string));
            let Some(placeholder) = placeholder else { return };
            let moved = control
                .call("session.move_block", json!({"session_id": session, "block_id": block, "anchor_block_id": placeholder}))
                .await;
            if moved.is_ok() {
                control.call("block.close", json!({"session_id": session, "block_id": placeholder})).await.ok();
                control.call("session.focus_block", json!({"session_id": session, "block_id": block})).await.ok();
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        done()
    }

    fn move_to_tab(&mut self, block: String, index: usize, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return done() };
        let Some(win) = view.windows.get(index.max(1) - 1) else { return fail("not_found", format!("there is no tab {index}")) };
        let anchor = win.focused_block_id.clone().or_else(|| tiled_leaves(win).first().map(|(id, _)| id.clone()));
        match anchor {
            Some(anchor) if anchor != block => {
                self.session(cx, "session.move_block", json!({"block_id": block, "anchor_block_id": anchor}))?;
                self.session(cx, "session.focus_block", json!({"block_id": block}))
            }
            _ => done(),
        }
    }

    fn change_font_size(&mut self, delta: Option<f32>, cx: &mut Context<Self>) {
        let size = match delta {
            Some(d) => (self.font_size + d).clamp(6.0, 72.0),
            None => self.base_font_size,
        };
        self.font_size = size;
        let panes: Vec<_> = self.store.read(cx).panes.values().cloned().collect();
        for pane in panes {
            pane.update(cx, |pane, cx| pane.set_font_size(size, cx));
        }
        self.store.update(cx, |s, _| s.settings.font_size = size);
    }

    fn open_path_input(&mut self, title: &str, action: &str, files: bool, cx: &mut Context<Self>) -> Outcome {
        let Some(view) = self.view(cx) else { return fail("conflict", "no session is open") };
        let Some(block) = focused_block(&view).map(str::to_string) else { return fail("conflict", "no pane is focused") };
        let start = self.focused_cwd(cx).map(|c| if c.ends_with('/') { c } else { c + "/" }).unwrap_or_else(|| "~/".into());
        let mut palette = Palette::input(title, action, json!({}), "path", &start).placeholder("A path. Tab completes.");
        palette.suggest = Suggest::Paths { session_id: view.session_id.clone().unwrap_or_default(), block_id: block, files };
        self.palette = Some(palette);
        self.refresh_suggestions(cx);
        done()
    }

    /// Lists the entries below the typed path, from the terminal's machine.
    pub fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(p) = &self.palette else { return };
        let Suggest::Paths { session_id, block_id, files } = p.suggest.clone() else { return };
        let query = p.query.clone();
        let (dir, prefix) = match query.rfind('/') {
            Some(i) => (query[..=i].to_string(), query[i + 1..].to_string()),
            None => (String::new(), query.clone()),
        };
        let Some(control) = self.store.read(cx).control() else { return };
        let args = json!({"path": if dir.is_empty() { "." } else { dir.as_str() }, "prefix": prefix, "files": files, "limit": 50});
        cx.spawn(async move |this, cx| {
            let result = control.block_call("com.superlogical.terminal.list_dir", &session_id, &block_id, args).await;
            this.update(cx, |this, cx| {
                let Some(p) = &mut this.palette else { return };
                if p.query != query {
                    return;
                }
                let entries = result.ok().and_then(|v| v["entries"].as_array().cloned()).unwrap_or_default();
                p.items = entries
                    .iter()
                    .filter_map(|e| {
                        let name = e["name"].as_str()?;
                        let is_dir = !e["file"].as_bool().unwrap_or(false);
                        let full = format!("{dir}{name}{}", if is_dir { "/" } else { "" });
                        let (action, arg) = match &p.kind {
                            crate::palette::Kind::Input { action, .. } => (action.clone(), full.clone()),
                            _ => return None,
                        };
                        Some(Item::new(full, action, json!({"path": arg})).detail(if is_dir { "directory" } else { "" }))
                    })
                    .collect();
                p.selected = 0;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // Palette contents.

    pub fn open_commands(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let km = &store.keymap;
        let mut items: Vec<Item> = Vec::new();
        let mut add = |a: &ActionInfo| {
            if a.title.is_empty() || a.needs_args() || a.name == "client.key.forward" {
                return;
            }
            let hint = km.keys_for(&a.name, &Value::Null).unwrap_or_default();
            let detail = if a.category.is_empty() { String::new() } else { a.category.clone() };
            items.push(Item::new(a.title.clone(), a.name.clone(), Value::Null).detail(detail).hint(hint).keywords(format!("{} {} {}", a.keywords, a.name, a.description)));
        };
        self.client_actions.iter().for_each(&mut add);
        km.server_actions.iter().for_each(&mut add);
        self.palette = Some(Palette::pick("Commands", items));
    }

    pub fn open_sessions(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let items = store
            .sessions
            .iter()
            .map(|s| {
                let tabs = store.views.get(&s.id).map(|v| format!("{} tabs", v.windows.len())).unwrap_or_default();
                let current = if store.current.as_ref() == Some(&s.id) { "current" } else { "" };
                Item::new(s.label.clone(), "session.select", json!({"session": s.id})).detail(tabs).hint(current)
            })
            .collect();
        self.palette = Some(Palette::pick("Change Session", items).empty("No session matches"));
    }

    /// Sessions, then the current session's tabs and panes.
    pub fn open_go_to(&mut self, cx: &mut Context<Self>) {
        let mut items = Vec::new();
        let store = self.store.read(cx);
        for s in &store.sessions {
            items.push(Item::new(s.label.clone(), "session.select", json!({"session": s.id})).detail("session"));
        }
        if let Some(view) = store.current_view() {
            for (i, w) in view.windows.iter().enumerate() {
                items.push(Item::new(self.window_title(w, cx), "client.tab.goto", json!({"index": i + 1})).detail(format!("tab {}", i + 1)));
                for layer in &w.layers {
                    for b in &layer.blocks {
                        let Some(id) = &b.block_id else { continue };
                        let title = store.panes.get(id).map(|p| p.read(cx).title.clone()).filter(|t| !t.is_empty());
                        let label = title.unwrap_or_else(|| b.label.clone());
                        items.push(Item::new(label, "pane.focus_block", json!({"block": id})).detail(format!("pane in tab {}", i + 1)).keywords(b.label.clone()));
                    }
                }
            }
        }
        self.palette = Some(Palette::pick("Go to", items));
    }
}

/// Quotes a path for a POSIX shell, if it needs quoting.
pub fn shell_quote(path: &str) -> String {
    if !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+~:@%,=".contains(c)) {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(json: &str) -> RexWindow {
        serde_json::from_str(json).unwrap()
    }

    const THREE: &str = r#"{"window_id":"w","focused_block_id":"a","layers":[{"kind":"tiled","layout":{"rect":[0,0,1,1],
        "split":{"direction":"horizontal","ratio":0.5,
          "before":{"rect":[0,0,0.5,1],"block_id":"a"},
          "after":{"rect":[0.5,0,0.5,1],"split":{"direction":"vertical","ratio":0.5,
             "before":{"rect":[0.5,0,0.5,0.5],"block_id":"b"},
             "after":{"rect":[0.5,0.5,0.5,0.5],"block_id":"c"}}}}}}]}"#;

    #[test]
    fn finds_neighbors_by_geometry() {
        let w = win(THREE);
        assert_eq!(neighbor(&w, "a", "right").as_deref(), Some("b"));
        assert_eq!(neighbor(&w, "c", "left").as_deref(), Some("a"));
        assert_eq!(neighbor(&w, "b", "down").as_deref(), Some("c"));
        assert_eq!(neighbor(&w, "a", "left"), None);
    }

    #[test]
    fn balances_by_pane_count() {
        let w = win(THREE);
        let mut out = Vec::new();
        balanced_ratios(w.layers[0].layout.as_ref().unwrap(), &mut out);
        // a | (b / c): side by side there are two columns, so half each.
        assert_eq!(out[0], ("horizontal".into(), "a".into(), "b".into(), 0.5));
        assert_eq!(out[1], ("vertical".into(), "b".into(), "c".into(), 0.5));
    }

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(shell_quote("/usr/bin"), "/usr/bin");
        assert_eq!(shell_quote("my file"), "'my file'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
