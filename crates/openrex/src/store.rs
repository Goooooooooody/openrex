//! The client's picture of the server: its sessions, the layout of the ones
//! it is attached to, and a pane for each terminal it shows.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use gpui::{AppContext, AsyncApp, Context, Entity, Task, WeakEntity};
use openrex_proto::control::{self, Control, ControlEvent, Hello};
use openrex_proto::types::{SessionView, Window};
use openrex_proto::Endpoint;
use serde_json::{Value, json};

use crate::keymap::{self, Keymap};
use crate::pane::Pane;

pub const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

#[derive(Clone, Debug)]
pub struct Settings {
    pub font_family: String,
    pub font_size: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
}

/// Another client's request, through `client.perform`, for OpenRex to
/// perform one of its actions.
#[derive(Clone, Debug)]
pub struct PerformRequest {
    pub call_id: String,
    pub action: String,
    pub args: Value,
    pub origin: String,
    pub session_id: Option<String>,
    pub block_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Disconnected(String),
}

pub struct Store {
    pub endpoint: Endpoint,
    pub settings: Settings,
    pub status: Status,
    pub client_id: Option<String>,
    pub sessions: Vec<SessionInfo>,
    /// The session the window shows.
    pub current: Option<String>,
    /// The layout of each attached session.
    pub views: HashMap<String, SessionView>,
    /// A pane for each terminal that has been shown, by block ID.
    pub panes: HashMap<String, Entity<Pane>>,
    /// The last failed request, for the status bar.
    pub error: Option<String>,
    /// The block the window should move keyboard focus to.
    focus_request: Option<String>,
    /// Text a program copied, waiting for the window to put it on the
    /// clipboard.
    pub pending_clipboard: Option<String>,
    /// The key bindings the server resolved for this client.
    pub keymap: Keymap,
    /// Requests from other clients to perform one of OpenRex's actions, for
    /// the window to perform and answer.
    pub perform_queue: Vec<PerformRequest>,
    /// A short message for the status bar, such as "Copied pane ID".
    pub notice: Option<String>,
    /// Hosts, Tailscale, and the server OpenRex started with.
    pub server: crate::server::ServerState,
    control: Option<Control>,
    fetching: HashSet<String>,
    refetch: HashSet<String>,
    attaching: HashSet<String>,
    pub(crate) connection: Option<Task<()>>,
}

fn shell_layout() -> Value {
    json!({"block": {"flavor": SHELL_FLAVOR}})
}

/// A layout of one shell, started in `cwd` when given.
pub fn shell_layout_in(cwd: Option<String>) -> Value {
    match cwd.filter(|c| !c.is_empty()) {
        Some(cwd) => json!({"block": {"flavor": SHELL_FLAVOR, "options": {"cwd": cwd}}}),
        None => shell_layout(),
    }
}

impl Store {
    pub fn new(endpoint: Endpoint, settings: Settings, cx: &mut Context<Self>) -> Self {
        let mut store = Store {
            endpoint,
            settings,
            status: Status::Connecting,
            client_id: None,
            sessions: Vec::new(),
            current: None,
            views: HashMap::new(),
            panes: HashMap::new(),
            error: None,
            focus_request: None,
            pending_clipboard: None,
            keymap: Keymap::defaults(),
            perform_queue: Vec::new(),
            notice: None,
            server: Default::default(),
            control: None,
            fetching: HashSet::new(),
            refetch: HashSet::new(),
            attaching: HashSet::new(),
            connection: None,
        };
        crate::server::close_on_quit(cx);
        store.connect(cx);
        store
    }

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Connecting;
        self.error = None;
        let endpoint = self.endpoint.clone();
        self.connection = Some(cx.spawn(async move |this, cx| {
            let why = match run(endpoint, this.clone(), cx).await {
                Ok(()) => "the server closed the connection".to_string(),
                Err(e) => format!("{e:#}"),
            };
            this.update(cx, |this, cx| {
                this.status = Status::Disconnected(why);
                this.control = None;
                this.panes.clear();
                this.views.clear();
                this.fetching.clear();
                this.refetch.clear();
                this.attaching.clear();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_bootstrap(&mut self, control: Control, boot: Value, cx: &mut Context<Self>) {
        self.control = Some(control);
        self.status = Status::Connected;
        self.client_id = boot["client_id"].as_str().map(str::to_string);
        self.sessions = parse_sessions(&boot["attach_all"]["sessions"]);
        let selected = &boot["selected_session"];
        if let Ok(view) = serde_json::from_value::<SessionView>(selected["view"].clone()) {
            if let Some(id) = view.session_id.clone() {
                self.current = Some(id.clone());
                self.apply_view(view, cx);
                self.own_blocks(&id, None, cx);
            }
        }
        if self.sessions.is_empty() {
            // A server with nothing running: start a first session.
            self.new_session(cx);
        }
        self.fetch_keymap(cx);
        cx.notify();
    }

    /// Declares OpenRex's actions and default keys, and reads back the
    /// bindings the server resolved with the configuration file.
    pub fn fetch_keymap(&mut self, cx: &mut Context<Self>) {
        let bindings: Vec<Value> = keymap::DEFAULT_BINDINGS
            .iter()
            .map(|(keys, action, args)| {
                let mut b = json!({"keys": keys.split('>').collect::<Vec<_>>(), "action": action});
                if !args.is_empty() {
                    b["args"] = serde_json::from_str(args).unwrap_or(Value::Null);
                }
                b
            })
            .collect();
        let actions: Vec<Value> = keymap::client_actions().iter().map(|a| a.to_json()).collect();
        let payload = json!({
            "actions": actions,
            "defaults": [{"name": "default", "bindings": bindings}],
        });
        self.call("client.keymap", payload, cx, |this, result, cx| match result {
            Ok(v) => {
                this.keymap = Keymap::from_result(&v);
                cx.notify();
            }
            Err(e) => this.error = Some(format!("client.keymap: {e}")),
        });
    }

    fn handle_event(&mut self, ev: ControlEvent, cx: &mut Context<Self>) {
        match ev.name.as_str() {
            "session_created" => {
                let s = &ev.payload["session"];
                if let Some(id) = s["session_id"].as_str() {
                    if !self.sessions.iter().any(|x| x.id == id) {
                        self.sessions.push(SessionInfo {
                            id: id.to_string(),
                            label: s["label"].as_str().unwrap_or("").to_string(),
                        });
                    }
                    if self.current.is_none() {
                        self.select_session(id.to_string(), cx);
                    }
                }
            }
            "session_destroyed" => {
                if let Some(id) = ev.payload["session_id"].as_str() {
                    self.forget_session(id, cx);
                }
            }
            "session_moved" => self.refresh_sessions(cx),
            "hosts_changed" | "tailscale_status_changed" => crate::server::on_event(self, &ev.name, &ev.payload, cx),
            "keymap_changed" => {
                if ev.payload["hash"].as_str() != Some(self.keymap.hash.as_str()) {
                    self.fetch_keymap(cx);
                }
            }
            "client_perform_requested" => {
                let p = &ev.payload;
                if let (Some(call_id), Some(action)) = (p["call_id"].as_str(), p["action"].as_str()) {
                    self.perform_queue.push(PerformRequest {
                        call_id: call_id.to_string(),
                        action: action.to_string(),
                        args: p.get("args").cloned().unwrap_or(Value::Null),
                        origin: p["origin"].as_str().unwrap_or("api").to_string(),
                        session_id: p["session_id"].as_str().map(str::to_string),
                        block_id: p["block_id"].as_str().map(str::to_string),
                    });
                }
            }
            "session_event" => {
                let Some(session_id) = ev.payload["session_id"].as_str() else { return };
                let event = &ev.payload["event"];
                let name = event["type"].as_str().unwrap_or("");
                self.handle_session_event(session_id, name, &event["payload"], cx);
            }
            _ => {}
        }
        cx.notify();
    }

    fn handle_session_event(&mut self, session_id: &str, name: &str, p: &Value, cx: &mut Context<Self>) {
        match name {
            "session_view_changed" => {
                let revision = p["revision"].as_u64().unwrap_or(u64::MAX);
                let known = self.views.get(session_id).map(|v| v.revision);
                if known.is_some_and(|k| revision > k) {
                    self.fetch_view(session_id.to_string(), cx);
                }
            }
            "session_label_changed" => self.refresh_sessions(cx),
            "block_created" => {
                if let Some(block_id) = p["block_id"].as_str() {
                    self.own_blocks(session_id, Some(block_id), cx);
                }
            }
            "block_closed" => {
                if let Some(block_id) = p["block_id"].as_str() {
                    self.panes.remove(block_id);
                }
            }
            "block_event" => {
                let Some(block_id) = p["block_id"].as_str() else { return };
                let payload = &p["payload"];
                match p["name"].as_str().unwrap_or("") {
                    "title_changed" => {
                        let title = payload["title"].as_str().unwrap_or("").to_string();
                        if let Some(pane) = self.panes.get(block_id) {
                            pane.update(cx, |pane, cx| {
                                pane.title = title;
                                cx.notify();
                            });
                        }
                    }
                    "pwd_changed" => {
                        if let (Some(pwd), Some(pane)) = (payload["pwd"].as_str(), self.panes.get(block_id)) {
                            let path = crate::pane::path_of_pwd(pwd);
                            pane.update(cx, |pane, _| pane.cwd = Some(path));
                        }
                    }
                    "clipboard_written" => {
                        // The window writes it: on Wayland only a focused
                        // window may set the clipboard.
                        if let Some(text) = payload["text"].as_str() {
                            self.pending_clipboard = Some(text.to_string());
                            cx.notify();
                        }
                    }
                    "desktop_notification" => {
                        let title = payload["title"].as_str().unwrap_or("");
                        let body = payload["body"].as_str().unwrap_or("");
                        notify_desktop(title, body);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn forget_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.sessions.retain(|s| s.id != id);
        self.views.remove(id);
        self.panes.retain(|_, pane| pane.read(cx).session_id != id);
        if self.current.as_deref() == Some(id) {
            self.current = None;
            if let Some(next) = self.sessions.first().map(|s| s.id.clone()) {
                self.select_session(next, cx);
            }
        }
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.control.clone() else { return };
        cx.spawn(async move |this, cx| {
            if let Ok(v) = control.call("session.list", json!({})).await {
                this.update(cx, |this, cx| {
                    this.sessions = parse_sessions(&v["sessions"]);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Shows a session, attaching to it first if this is the first time.
    pub fn select_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.current = Some(id.clone());
        if let Some(view) = self.views.get(&id).cloned() {
            self.apply_view(view, cx);
        } else if self.attaching.insert(id.clone()) {
            let Some(control) = self.control.clone() else { return };
            cx.spawn(async move |this, cx| {
                let result = control.call("session.attach", json!({"session_id": id})).await;
                this.update(cx, |this, cx| {
                    this.attaching.remove(&id);
                    match result.map(|v| serde_json::from_value::<SessionView>(v["view"].clone())) {
                        Ok(Ok(view)) => {
                            this.apply_view(view, cx);
                            this.own_blocks(&id, None, cx);
                        }
                        Ok(Err(e)) => this.error = Some(format!("session.attach: {e}")),
                        Err(e) => this.error = Some(format!("session.attach: {}", e.message)),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn fetch_view(&mut self, session_id: String, cx: &mut Context<Self>) {
        if !self.fetching.insert(session_id.clone()) {
            // A fetch is on its way. Fetch again after it, to see this change.
            self.refetch.insert(session_id);
            return;
        }
        let Some(control) = self.control.clone() else { return };
        cx.spawn(async move |this, cx| {
            let result = control.call("session.view", json!({"session_id": session_id})).await;
            this.update(cx, |this, cx| {
                this.fetching.remove(&session_id);
                if let Ok(Ok(view)) = result.map(serde_json::from_value::<SessionView>) {
                    let newer = this.views.get(&session_id).is_none_or(|v| view.revision > v.revision);
                    if newer {
                        this.apply_view(view, cx);
                    }
                }
                if this.refetch.remove(&session_id) {
                    this.fetch_view(session_id.clone(), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_view(&mut self, view: SessionView, cx: &mut Context<Self>) {
        let Some(session_id) = view.session_id.clone() else { return };
        let before = self.views.get(&session_id).and_then(focused_block).map(str::to_string);

        // A pane for every terminal in the window the session shows.
        if let Some(window) = active_window(&view) {
            for layer in &window.layers {
                for block in &layer.blocks {
                    let Some(block_id) = &block.block_id else { continue };
                    if block.creator_name != "com.superlogical.terminal" {
                        continue;
                    }
                    if !self.panes.contains_key(block_id) {
                        if let Some(control) = self.control.clone() {
                            let (endpoint, settings) = (self.endpoint.clone(), self.settings.clone());
                            let (sid, bid) = (session_id.clone(), block_id.clone());
                            let pane = cx.new(|cx| Pane::new(sid, bid, control, endpoint, settings, cx));
                            self.panes.insert(block_id.clone(), pane);
                        }
                    }
                }
            }
        }
        // Drop panes whose blocks left the session.
        let alive: HashSet<&str> = view
            .windows
            .iter()
            .flat_map(|w| &w.layers)
            .flat_map(|l| &l.blocks)
            .filter_map(|b| b.block_id.as_deref())
            .collect();
        self.panes.retain(|id, pane| pane.read(cx).session_id != session_id || alive.contains(id.as_str()));

        let after = focused_block(&view).map(str::to_string);
        if self.current.as_deref() == Some(&session_id) && (after != before || self.focus_request.is_none()) {
            self.focus_request = after;
        }
        self.views.insert(session_id, view);
        cx.notify();
    }

    /// Claims blocks, so that the terminals take this client's size.
    pub fn own_blocks(&mut self, session_id: &str, block_id: Option<&str>, cx: &mut Context<Self>) {
        let mut payload = json!({"session_id": session_id, "mode": "take"});
        if let Some(block_id) = block_id {
            payload["block_ids"] = json!([block_id]);
        }
        self.request("session.own_block", payload, cx);
    }

    /// Calls a method for its effect. A failure shows in the status bar.
    pub fn request(&mut self, method: &'static str, payload: Value, cx: &mut Context<Self>) {
        let Some(control) = self.control.clone() else { return };
        cx.spawn(async move |this, cx| {
            let result = control.call(method, payload).await;
            this.update(cx, |this, cx| {
                this.error = result.err().map(|e| format!("{method}: {}", e.message));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Calls a method and hands its result to `done` on the store.
    pub fn call(
        &mut self,
        method: &'static str,
        payload: Value,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Store, Result<Value, control::RpcError>, &mut Context<Store>) + 'static,
    ) {
        let Some(control) = self.control.clone() else { return };
        cx.spawn(async move |this, cx| {
            let result = control.call(method, payload).await;
            this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Calls a method for its effect, in the current session. A failure shows
    /// in the status bar.
    pub fn session_request(&mut self, method: &'static str, mut payload: Value, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        payload["session_id"] = json!(id);
        self.request(method, payload, cx);
    }

    pub fn control(&self) -> Option<Control> {
        self.control.clone()
    }

    /// Answers another client's `client.perform` request.
    pub fn reply(&self, call_id: &str, result: Result<Value, (String, String)>) {
        let Some(control) = &self.control else { return };
        let payload = match result {
            Ok(v) => json!({"call_id": call_id, "result": v}),
            Err((code, message)) => json!({"call_id": call_id, "error": {"code": code, "message": message}}),
        };
        control.send_nowait("client.reply", payload);
    }

    /// Asks for the session view again, as after a missed change.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.refresh_sessions(cx);
        let ids: Vec<String> = self.views.keys().cloned().collect();
        for id in ids {
            self.fetch_view(id, cx);
        }
    }

    pub fn take_focus_request(&mut self) -> Option<String> {
        self.focus_request.take()
    }

    pub fn current_view(&self) -> Option<&SessionView> {
        self.views.get(self.current.as_deref()?)
    }

    pub fn focused_pane(&self) -> Option<Entity<Pane>> {
        self.panes.get(focused_block(self.current_view()?)?).cloned()
    }

    // Actions.

    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.control.clone() else { return };
        cx.spawn(async move |this, cx| {
            let payload = json!({"initial_windows": [{"layout": shell_layout()}]});
            let result = control.call("session.create", payload).await;
            this.update(cx, |this, cx| match result {
                Ok(v) => {
                    if let Some(id) = v["session_id"].as_str() {
                        this.select_session(id.to_string(), cx);
                        this.refresh_sessions(cx);
                    }
                }
                Err(e) => {
                    this.error = Some(format!("session.create: {}", e.message));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn destroy_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.request("session.destroy", json!({"session_id": id}), cx);
    }

    pub fn new_window(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return self.new_session(cx) };
        self.request("session.new_window", json!({"session_id": id, "layout": shell_layout()}), cx);
    }

    /// Splits the focused pane. `direction` is `horizontal` for side by side
    /// or `vertical` for one above the other.
    pub fn split(&mut self, direction: &str, cx: &mut Context<Self>) {
        self.split_side(direction, "after", None, cx);
    }

    /// Splits the focused pane, putting the new one `before` (left or above)
    /// or `after` it, starting in `cwd` if given.
    pub fn split_side(&mut self, direction: &str, side: &str, cwd: Option<String>, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        let payload = json!({
            "session_id": id,
            "direction": direction,
            "side": side,
            "layout": shell_layout_in(cwd),
            "focus": true,
        });
        self.request("session.new_split", payload, cx);
    }

    /// Opens a new tab whose shell starts in `cwd`.
    pub fn new_window_in(&mut self, cwd: Option<String>, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return self.new_session(cx) };
        self.request("session.new_window", json!({"session_id": id, "layout": shell_layout_in(cwd)}), cx);
    }

    pub fn close_focused_pane(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        let Some(block) = self.current_view().and_then(focused_block).map(str::to_string) else { return };
        self.request("block.close", json!({"session_id": id, "block_id": block}), cx);
    }

    pub fn close_window(&mut self, window_id: String, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        self.request("session.close_window", json!({"session_id": id, "window_id": window_id}), cx);
    }

    pub fn focus_window(&mut self, window_id: String, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        self.request("session.focus_window", json!({"session_id": id, "window_id": window_id}), cx);
    }

    pub fn cycle_window(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        let method = if forward { "session.focus_next_window" } else { "session.focus_previous_window" };
        self.request(method, json!({"session_id": id}), cx);
    }

    pub fn focus_direction(&mut self, direction: &str, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        self.request("session.focus_direction", json!({"session_id": id, "direction": direction}), cx);
    }

    pub fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        self.request("session.zoom_block", json!({"session_id": id}), cx);
    }

    /// Moves the border between two panes.
    pub fn resize_split(&mut self, direction: &str, before: &str, after: &str, ratio: f32, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        let payload = json!({
            "session_id": id,
            "direction": direction,
            "before_block_id": before,
            "after_block_id": after,
            "ratio": ratio.clamp(0.05, 0.95),
        });
        self.request("session.resize_pane", payload, cx);
    }
}

async fn run(endpoint: Endpoint, this: WeakEntity<Store>, cx: &mut AsyncApp) -> Result<()> {
    let hello = Hello {
        name: "OpenRex".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        label: crate::server::client_label(),
        ..Default::default()
    };
    let conn = control::connect(&endpoint, hello).await?;
    let control = conn.control.clone();
    let driver = cx.background_executor().spawn(conn.driver);

    let boot = control.call("session.bootstrap", json!({"event_scope": "all"})).await?;
    this.update(cx, |this, cx| this.on_bootstrap(control.clone(), boot, cx))?;

    while let Ok(event) = conn.events.recv().await {
        this.update(cx, |this, cx| this.handle_event(event, cx))?;
    }
    driver.await
}

fn parse_sessions(v: &Value) -> Vec<SessionInfo> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(SessionInfo {
                id: s["session_id"].as_str()?.to_string(),
                label: s["label"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

pub fn active_window(view: &SessionView) -> Option<&Window> {
    let id = view.active_window_id.as_deref()?;
    view.windows.iter().find(|w| w.window_id.as_deref() == Some(id))
}

pub fn focused_block(view: &SessionView) -> Option<&str> {
    active_window(view)?.focused_block_id.as_deref()
}

fn notify_desktop(title: &str, body: &str) {
    let (summary, body) = if title.is_empty() { (body, "") } else { (title, body) };
    let (summary, body) = (summary.to_string(), body.to_string());
    // Wait for the command on its own thread, so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        std::process::Command::new("notify-send")
            .args(["--app-name=OpenRex", "--", &summary, &body])
            .status()
            .ok();
    });
}
