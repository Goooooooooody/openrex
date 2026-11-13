//! The server and its hosts: client.diagnostics, client.server.status, client.clients, Tailscale, client.version.copy, client.debug.copy, and client.host.*.
//!
//! Diagnostics ask the server about itself when they open, so they always
//! show the server the window is connected to. Hosts are the other Rex
//! servers people connect to by label. They live on the home server, the one
//! OpenRex started with, so OpenRex remembers that server's endpoint when it
//! moves to a host, both to come back and to keep reading and changing the
//! host list there.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use gpui::{App, AsyncApp, Context, Task, Window};
use openrex_proto::Endpoint;
use openrex_proto::control::{self, Control, Hello};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target};
use crate::palette::{Item, Palette};
use crate::store::{Status, Store};
use crate::workspace::Workspace;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a host's address has to answer before the next one is tried.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// The palette action that copies a diagnostics row. It isn't declared to
/// the server: only the palette's own rows perform it.
const COPY_ROW: &str = "client.diagnostics.copy";

/// What OpenRex knows about the server beyond its sessions.
#[derive(Clone, Debug, Default)]
pub struct ServerState {
    /// The endpoint OpenRex started with, kept once the window moves to a
    /// host, so it can come back.
    pub home: Option<Endpoint>,
    /// The host the window shows, by label. `None` on the home server.
    pub host: Option<String>,
    /// The home server's hosts, as it last listed them.
    pub hosts: Vec<Host>,
    pub hosts_hash: String,
    /// The connected server's Tailscale status, as it last reported it.
    pub tailscale: Option<Value>,
    /// A Tailscale sign-in was asked for, and its link hasn't arrived yet.
    login_pending: bool,
}

/// One entry of the home server's host list.
#[derive(Clone, Debug, PartialEq)]
pub struct Host {
    pub id: String,
    pub label: String,
    /// Addresses in the forms `rex --server` takes, in the order to try them.
    pub endpoints: Vec<String>,
}

/// Where a host switch goes: the home server, or a host's addresses.
#[derive(Clone, Debug, PartialEq)]
pub struct Destination {
    /// The host's label, or `None` for the home server.
    pub label: Option<String>,
    pub candidates: Vec<Endpoint>,
}

static CLIENT_LABEL: OnceLock<String> = OnceLock::new();

/// Sets the label OpenRex gives itself when it connects, so that people can
/// address it with `rex -C LABEL`. Only the first call counts.
pub fn set_client_label(label: String) {
    CLIENT_LABEL.set(label).ok();
}

/// The label from `--label`, for the hello of every connection.
pub fn client_label() -> Option<String> {
    CLIENT_LABEL.get().cloned()
}

/// Checks a label the way the server does, so a bad `--label` fails at
/// start instead of being dropped by the server without a word.
pub fn check_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() {
        return Err("--label needs a label, such as studio".into());
    }
    if label.len() > 64 {
        return Err(format!("--label takes at most 64 bytes, and {label} has {}", label.len()));
    }
    if label.chars().any(char::is_control) {
        return Err("--label can't hold control characters".into());
    }
    Ok(label.to_string())
}

/// Closes the control connection when OpenRex quits, however it quits, so
/// the server forgets this client at once instead of keeping it as a lost
/// connection that may come back.
pub fn close_on_quit(cx: &mut Context<Store>) {
    cx.on_app_quit(|store: &mut Store, _| {
        let control = store.control();
        // Give up the blocks this client sized, so another client showing
        // them can size them instead.
        let sessions: Vec<String> = store.views.keys().cloned().collect();
        async move {
            if let Some(control) = control {
                for session in sessions {
                    control.send_nowait("session.disown_block", serde_json::json!({"session_id": session}));
                }
                control.close();
                // The connection's writer runs on another thread. Give it a
                // moment to send the close frame before the process ends.
                smol::Timer::after(Duration::from_millis(60)).await;
            }
        }
    })
    .detach();
}

/// Reads what the server just connected to keeps about itself. Hosts come
/// from the home server, so only there.
pub fn on_connected(store: &mut Store, cx: &mut Context<Store>) {
    if store.server.host.is_none() {
        fetch_hosts(store, cx);
    }
}

/// Follows the events about the server itself.
pub fn on_event(store: &mut Store, name: &str, payload: &Value, cx: &mut Context<Store>) {
    match name {
        "hosts_changed" => {
            // A host's own list doesn't matter while the window shows it:
            // hosts come from the home server.
            if store.server.host.is_none() && payload["hash"].as_str() != Some(store.server.hosts_hash.as_str()) {
                fetch_hosts(store, cx);
            }
        }
        "tailscale_status_changed" => {
            store.server.tailscale = Some(payload.clone());
            if store.server.login_pending {
                if let Some(url) = payload["auth_url"].as_str().filter(|u| !u.is_empty()) {
                    give_sign_in_link(store, url);
                } else if !wants_sign_in(s(payload, "state")) {
                    // Signed in, or off: no link is coming. Don't open one
                    // that a later sign-in, asked for elsewhere, makes.
                    store.server.login_pending = false;
                }
            }
        }
        _ => {}
    }
}

fn fetch_hosts(store: &mut Store, cx: &mut Context<Store>) {
    store.call("hosts.list", json!({}), cx, |s, result, _| {
        if let Ok(v) = result {
            s.server.hosts = parse_hosts(&v);
            s.server.hosts_hash = v["hash"].as_str().unwrap_or("").to_string();
        }
    });
}

/// Reads the hosts of a `hosts.list` result, skipping any without an ID or
/// a label.
pub fn parse_hosts(v: &Value) -> Vec<Host> {
    v["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(Host {
                id: h["id"].as_str().filter(|s| !s.is_empty())?.to_string(),
                label: h["label"].as_str().filter(|s| !s.is_empty())?.to_string(),
                endpoints: h["endpoints"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
            })
        })
        .collect()
}

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    _target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    Some(match action {
        "client.diagnostics" => open_report(ws, Report::All, cx),
        "client.server.status" => open_report(ws, Report::Status, cx),
        "client.clients" => open_report(ws, Report::Clients, cx),
        COPY_ROW => match arg(args, "text") {
            Some(text) => {
                ws.copy_text(text.to_string(), "Copied", cx);
                Ok(Value::Null)
            }
            None => fail("bad_request", "text is required"),
        },
        "client.version.copy" => copy_version(ws, cx),
        "client.debug.copy" => copy_debug(ws, cx),
        "client.tailscale.enable" => tailscale_enable(ws, args, cx),
        "client.tailscale.disable" => tailscale_call(ws, "server.tailscale.disable", json!({}), cx),
        "client.tailscale.login" => tailscale_call(ws, "server.tailscale.login", json!({}), cx),
        "client.host.switch" => host_switch(ws, arg(args, "host"), cx),
        "client.host.add" => host_add(ws, arg(args, "address"), arg(args, "label"), cx),
        "client.host.remove" => host_remove(ws, arg(args, "host"), cx),
        _ => return None,
    })
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn fail(code: &str, message: impl Into<String>) -> Outcome {
    Err((code.to_string(), message.into()))
}

fn control_of(ws: &Workspace, cx: &App) -> Result<Control, (String, String)> {
    ws.store.read(cx).control().ok_or_else(|| ("conflict".to_string(), "OpenRex isn't connected to a server".to_string()))
}

fn show_error(ws: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    ws.store.update(cx, |s, cx| {
        s.notice = None;
        s.error = Some(message);
        cx.notify();
    });
}

fn show_notice(store: &mut Store, message: impl Into<String>) {
    store.error = None;
    store.notice = Some(message.into());
}

// Diagnostics.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Report {
    /// Everything: the server, the keymap, and the clients.
    All,
    /// The server alone.
    Status,
    /// The connected clients alone.
    Clients,
}

impl Report {
    fn title(self) -> &'static str {
        match self {
            Report::All => "Rex Diagnostics",
            Report::Status => "Server Status",
            Report::Clients => "Connected Clients",
        }
    }
}

/// What the server said about itself. Each part is its result, or why the
/// call failed.
#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    pub version: Option<Result<Value, String>>,
    pub status: Option<Result<Value, String>>,
    pub whoami: Option<Result<Value, String>>,
    pub clients: Option<Result<Value, String>>,
}

/// What OpenRex knows without asking.
#[derive(Clone, Debug, Default)]
pub struct LocalInfo {
    pub endpoint: String,
    pub host: Option<String>,
    pub client_id: Option<String>,
    pub keymap_path: String,
    pub keymap_exists: bool,
    pub keymap_diagnostics: Vec<String>,
}

impl LocalInfo {
    fn of(store: &Store) -> LocalInfo {
        LocalInfo {
            endpoint: store.endpoint.to_string(),
            host: store.server.host.clone(),
            client_id: store.client_id.clone(),
            keymap_path: store.keymap.config_path.clone(),
            keymap_exists: store.keymap.config_exists,
            keymap_diagnostics: store.keymap.diagnostics.clone(),
        }
    }
}

/// One line of a report: a section, the line itself, and its details.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub section: &'static str,
    pub label: String,
    pub detail: String,
    /// Shown on the right instead of the section, such as "this client".
    pub mark: String,
}

impl Row {
    fn new(section: &'static str, label: impl Into<String>, detail: impl Into<String>) -> Row {
        Row { section, label: label.into(), detail: detail.into(), mark: String::new() }
    }

    /// The row as text, for the clipboard.
    pub fn text(&self) -> String {
        if self.detail.is_empty() { self.label.clone() } else { format!("{}: {}", self.label, self.detail) }
    }
}

async fn ask(control: &Control, method: &str, wanted: bool) -> Option<Result<Value, String>> {
    if !wanted {
        return None;
    }
    Some(control.call(method, json!({})).await.map_err(|e| e.message))
}

/// Asks the server what a report needs, all at once.
async fn fetch_info(control: Control, report: Report) -> ServerInfo {
    let server = report != Report::Clients;
    let clients = report != Report::Status;
    let (version, status, whoami, list) = futures::join!(
        ask(&control, "server.version", server),
        ask(&control, "server.status", server),
        ask(&control, "server.whoami", server),
        ask(&control, "client.list", clients),
    );
    ServerInfo { version, status, whoami, clients: list }
}

fn open_report(ws: &mut Workspace, report: Report, cx: &mut Context<Workspace>) -> Outcome {
    let control = control_of(ws, cx)?;
    let title = report.title();
    ws.palette = Some(Palette::pick(title, Vec::new()).empty("Asking the server…"));
    let local = LocalInfo::of(ws.store.read(cx));
    cx.spawn(async move |this, cx| {
        let info = fetch_info(control, report).await;
        this.update(cx, |ws, cx| {
            if let Some(status) = info.status.as_ref().and_then(|s| s.as_ref().ok()) {
                let tailscale = status.get("tailscale").cloned();
                ws.store.update(cx, |s, _| s.server.tailscale = tailscale);
            }
            let Some(p) = &mut ws.palette else { return };
            if p.title != title {
                return;
            }
            p.items = report_rows(&info, &local, report)
                .into_iter()
                .map(|row| {
                    let hint = if row.mark.is_empty() { row.section.to_string() } else { row.mark.clone() };
                    let text = row.text();
                    Item::new(row.label, COPY_ROW, json!({"text": text})).detail(row.detail).hint(hint).keywords(row.section)
                })
                .collect();
            p.empty = "Nothing to show".into();
            cx.notify();
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Joins the parts that aren't empty.
fn join(parts: &[String]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join(", ")
}

/// An RFC 3339 time without its fraction of a second.
pub fn short_time(t: &str) -> String {
    match (t.find('.'), t.find(['Z', '+']).or_else(|| t.rfind('-').filter(|&i| i > 10))) {
        (Some(dot), Some(zone)) if dot < zone => format!("{}{}", &t[..dot], &t[zone..]),
        _ => t.to_string(),
    }
}

fn short_commit(c: &str) -> &str {
    c.get(..7).unwrap_or(c)
}

/// The server's version and build in one line, such as
/// `0.0.17-g2283 (linux_amd64, commit 2283581)`.
pub fn version_line(v: &Value) -> String {
    let mut build = vec![s(v, "platform").to_string()];
    if !s(v, "commit").is_empty() {
        build.push(format!("commit {}", short_commit(s(v, "commit"))));
    }
    if v["dirty"].as_bool() == Some(true) {
        build.push("dirty".into());
    }
    let build = join(&build);
    if build.is_empty() { s(v, "rex_version").to_string() } else { format!("{} ({build})", s(v, "rex_version")) }
}

/// The state of built-in Tailscale in words.
pub fn tailscale_state(t: &Value) -> &'static str {
    match s(t, "state") {
        "disabled" => "off",
        "needs_login" => "needs sign-in",
        "needs_machine_approval" => "waiting for a tailnet admin to approve the device",
        "running" if s(&t["https"], "state") == "available" => "on",
        "running" => "on, HTTPS pending",
        _ => "starting",
    }
}

fn tailscale_row(t: Option<&Value>) -> Row {
    let Some(t) = t else {
        return Row::new("tailscale", "Tailscale: not available", "this server can't run built-in Tailscale");
    };
    let detail = join(&[
        s(&t["https"], "url").to_string(),
        if s(&t["https"], "url").is_empty() { s(t, "dns_name").to_string() } else { String::new() },
        s(&t["account"], "login_name").to_string(),
        if s(t, "tailnet_name").is_empty() { String::new() } else { format!("tailnet {}", s(t, "tailnet_name")) },
        if t["allow_tailnet_members"].as_bool() == Some(true) { "tailnet members allowed".into() } else { String::new() },
        s(t, "control_url").to_string(),
        if s(t, "error").is_empty() { String::new() } else { format!("error: {}", s(t, "error")) },
    ]);
    Row::new("tailscale", format!("Tailscale: {}", tailscale_state(t)), detail)
}

/// The rows of a report.
pub fn report_rows(info: &ServerInfo, local: &LocalInfo, report: Report) -> Vec<Row> {
    let mut rows = Vec::new();
    let failed = |section, what: &str, e: &String| Row::new(section, format!("{what}: unknown"), e.clone());
    if report != Report::Clients {
        match &info.version {
            Some(Ok(v)) => {
                let mut detail = vec![];
                if let Some(t) = v["built_at"].as_str() {
                    detail.push(format!("built {}", short_time(t)));
                }
                if let Some(r) = v["api_revision"].as_u64() {
                    detail.push(format!("API revision {r}"));
                }
                rows.push(Row::new("server", format!("Rex server {}", version_line(v)), join(&detail)));
            }
            Some(Err(e)) => rows.push(failed("server", "Rex server version", e)),
            None => {}
        }
        rows.push(Row::new("app", format!("OpenRex {VERSION}"), format!("this app, on {}/{}", std::env::consts::OS, std::env::consts::ARCH)));
        let place = match &local.host {
            Some(h) => format!("Connected to host {h}"),
            None => "Connected to this machine's server".to_string(),
        };
        rows.push(Row::new("server", place, local.endpoint.clone()));
        match &info.status {
            Some(Ok(st)) => {
                let sessions = st["session_count"].as_u64().unwrap_or(0);
                rows.push(Row::new(
                    "server",
                    format!("{sessions} session{}", if sessions == 1 { "" } else { "s" }),
                    join(&[
                        format!("server pid {}", st["pid"].as_u64().unwrap_or(0)),
                        format!("started {}", short_time(s(st, "started_at"))),
                        if st["exit_empty"].as_bool() == Some(true) { "stops when idle".into() } else { String::new() },
                    ]),
                ));
                rows.push(Row::new(
                    "server",
                    "Identity",
                    join(&[
                        if s(st, "host_id").is_empty() { String::new() } else { format!("host {}", s(st, "host_id")) },
                        format!("instance {}", s(st, "instance_id")),
                    ]),
                ));
                let config = &st["config"];
                let hash = s(config, "hash");
                rows.push(Row::new(
                    "config",
                    format!("Config loaded {} time{}", config["generation"].as_u64().unwrap_or(0), if config["generation"].as_u64() == Some(1) { "" } else { "s" }),
                    if hash.is_empty() { "no file, or it couldn't be read".to_string() } else { format!("hash {}", hash.get(..12).unwrap_or(hash)) },
                ));
                for l in st["listeners"].as_array().into_iter().flatten() {
                    let primary = if l["primary"].as_bool() == Some(true) { "primary".to_string() } else { String::new() };
                    rows.push(Row::new("listener", format!("Listening on {}", s(l, "endpoint")), join(&[s(l, "transport").into(), s(l, "admission").into(), primary])));
                }
                rows.push(tailscale_row(st.get("tailscale").filter(|t| t.is_object())));
            }
            Some(Err(e)) => rows.push(failed("server", "Server status", e)),
            None => {}
        }
        match &info.whoami {
            Some(Ok(w)) => {
                let p = &w["principal"];
                let mut detail = vec![format!("over {}", s(p, "transport"))];
                if !s(p, "remote_addr").is_empty() && s(p, "remote_addr") != "@" {
                    detail.push(format!("from {}", s(p, "remote_addr")));
                }
                if !s(w, "account").is_empty() {
                    detail.push(format!("the server runs as {}", s(w, "account")));
                }
                rows.push(Row::new("you", format!("You are {}", s(p, "display")), join(&detail)));
            }
            Some(Err(e)) => rows.push(failed("you", "Who you are", e)),
            None => {}
        }
    }
    if report == Report::All {
        let (label, detail) = if local.keymap_path.is_empty() {
            ("Config file unknown".to_string(), "the server hasn't sent a keymap yet".to_string())
        } else {
            (local.keymap_path.clone(), if local.keymap_exists { "the keymap's config file".into() } else { "the keymap's config file, which doesn't exist yet".into() })
        };
        rows.push(Row::new("keymap", label, detail));
        if local.keymap_diagnostics.is_empty() {
            rows.push(Row::new("keymap", "No keymap problems", ""));
        }
        for d in &local.keymap_diagnostics {
            rows.push(Row::new("keymap", d.clone(), ""));
        }
    }
    if report != Report::Status {
        match &info.clients {
            Some(Ok(v)) => {
                for c in v["clients"].as_array().into_iter().flatten() {
                    rows.push(client_row(c, local.client_id.as_deref()));
                }
            }
            Some(Err(e)) => rows.push(failed("client", "Clients", e)),
            None => {}
        }
    }
    rows
}

fn client_row(c: &Value, me: Option<&str>) -> Row {
    let info = &c["info"];
    let principal = &c["principal"];
    let name = if s(info, "name").is_empty() { s(c, "client_id") } else { s(info, "name") };
    let label = match s(info, "version") {
        "" => name.to_string(),
        v => format!("{name} {v}"),
    };
    let sessions = c["session_ids"].as_array().map_or(0, Vec::len);
    let lost = s(c, "connection_state") == "lost";
    let detail = join(&[
        if s(info, "label").is_empty() { String::new() } else { format!("label {}", s(info, "label")) },
        match (s(info, "kind"), s(info, "platform")) {
            ("", "") => String::new(),
            (k, "") => k.to_string(),
            ("", p) => format!("on {p}"),
            (k, p) => format!("{k} on {p}"),
        },
        format!("{} over {}", s(principal, "display"), s(principal, "transport")),
        if sessions > 0 { format!("{sessions} session{}", if sessions == 1 { "" } else { "s" }) } else { String::new() },
        if lost { "connection lost".into() } else { format!("connected {}", short_time(s(c, "connected_at"))) },
        s(c, "client_id").to_string(),
    ]);
    let mut row = Row::new("client", label, detail);
    if me.is_some_and(|id| id == s(c, "client_id")) {
        row.mark = "this client".into();
    } else if lost {
        row.mark = "lost".into();
    }
    row
}

// Version and debug info.

fn copy_version(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let control = control_of(ws, cx)?;
    cx.spawn(async move |this, cx| {
        let result = control.call("server.version", json!({})).await;
        this.update(cx, |ws, cx| match result {
            Ok(v) => ws.copy_text(format!("OpenRex {VERSION} / Rex server {}", version_line(&v)), "Copied the version info", cx),
            Err(e) => show_error(ws, format!("server.version: {}", e.message), cx),
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// What a debug report says about the client's own state.
#[derive(Clone, Debug, Default)]
pub struct DebugState {
    pub endpoint: String,
    pub host: Option<String>,
    pub status: String,
    pub client_id: String,
    pub label: Option<String>,
    pub os: String,
    pub display: String,
    pub sessions: usize,
    pub tabs: usize,
    pub panes: usize,
    pub open_panes: usize,
    pub keymap_path: String,
    pub keymap_exists: bool,
    pub bindings: usize,
    pub diagnostics: usize,
}

/// A plain-text report to paste into a bug report.
pub fn debug_report(d: &DebugState, version: Option<&Value>, status: Option<&Value>) -> String {
    let mut out = vec![format!("OpenRex {VERSION}")];
    out.push(match version {
        Some(v) => format!("Rex server {}", version_line(v)),
        None => "Rex server: unknown".into(),
    });
    if let Some(r) = version.and_then(|v| v["api_revision"].as_u64()) {
        out.push(format!("API revision: {r} (OpenRex speaks {})", control::API_REVISION));
    }
    out.push(format!("Endpoint: {}", d.endpoint));
    out.push(format!("Host: {}", d.host.as_deref().unwrap_or("this machine's server")));
    out.push(format!("Connection: {}", d.status));
    if !d.client_id.is_empty() {
        out.push(format!("Client: {}{}", d.client_id, d.label.as_ref().map(|l| format!(" (label {l})")).unwrap_or_default()));
    }
    out.push(format!("OS: {}", d.os));
    if !d.display.is_empty() {
        out.push(format!("Display: {}", d.display));
    }
    let server_sessions = status.and_then(|s| s["session_count"].as_u64()).map(|n| format!(" ({n} on the server)")).unwrap_or_default();
    out.push(format!("Sessions: {}{server_sessions}", d.sessions));
    out.push(format!("Current session: {} tabs, {} panes", d.tabs, d.panes));
    out.push(format!("Terminals open: {}", d.open_panes));
    if let Some(t) = status.and_then(|s| s.get("tailscale")).filter(|t| t.is_object()) {
        out.push(format!("Tailscale: {}", tailscale_state(t)));
    }
    let keymap = if d.keymap_path.is_empty() {
        "defaults only, the server hasn't sent a keymap".to_string()
    } else {
        format!("{} ({})", d.keymap_path, if d.keymap_exists { "exists" } else { "no file" })
    };
    out.push(format!("Keymap: {keymap}, {} bindings, {} diagnostics", d.bindings, d.diagnostics));
    out.join("\n") + "\n"
}

/// The `PRETTY_NAME` of an `os-release` file.
pub fn os_pretty_name(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|l| {
        let v = l.strip_prefix("PRETTY_NAME=")?;
        Some(v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty())
    })
}

fn os_description() -> String {
    let name = std::fs::read_to_string("/etc/os-release").ok().and_then(|t| os_pretty_name(&t)).unwrap_or_else(|| std::env::consts::OS.to_string());
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|k| format!(", kernel {}", k.trim())).unwrap_or_default();
    format!("{name} ({}{kernel})", std::env::consts::ARCH)
}

fn display_description() -> String {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let session = if session.is_empty() && std::env::var_os("WAYLAND_DISPLAY").is_some() { "wayland".to_string() } else { session };
    join(&[session, desktop])
}

fn debug_state(ws: &Workspace, cx: &App) -> DebugState {
    let store = ws.store.read(cx);
    let view = store.current_view();
    DebugState {
        endpoint: store.endpoint.to_string(),
        host: store.server.host.clone(),
        status: match &store.status {
            Status::Connected => "connected".into(),
            Status::Connecting => "connecting".into(),
            Status::Disconnected(why) => format!("disconnected: {why}"),
        },
        client_id: store.client_id.clone().unwrap_or_default(),
        label: client_label(),
        os: os_description(),
        display: display_description(),
        sessions: store.sessions.len(),
        tabs: view.map_or(0, |v| v.windows.len()),
        panes: view.map_or(0, |v| v.windows.iter().flat_map(|w| &w.layers).map(|l| l.blocks.len()).sum()),
        open_panes: store.panes.len(),
        keymap_path: store.keymap.config_path.clone(),
        keymap_exists: store.keymap.config_exists,
        bindings: store.keymap.bindings.len(),
        diagnostics: store.keymap.diagnostics.len(),
    }
}

fn copy_debug(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let state = debug_state(ws, cx);
    // Without a connection the report still says what OpenRex knows.
    let Some(control) = ws.store.read(cx).control() else {
        ws.copy_text(debug_report(&state, None, None), "Copied the debug info", cx);
        return Ok(Value::Null);
    };
    cx.spawn(async move |this, cx| {
        let (version, status) = futures::join!(control.call("server.version", json!({})), control.call("server.status", json!({})));
        this.update(cx, |ws, cx| {
            let report = debug_report(&state, version.as_ref().ok(), status.as_ref().ok());
            ws.copy_text(report, "Copied the debug info", cx);
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

// Tailscale.

fn tailscale_enable(ws: &mut Workspace, args: &Value, cx: &mut Context<Workspace>) -> Outcome {
    let control = control_of(ws, cx)?;
    let allow = args.get("allow_tailnet_members").and_then(Value::as_bool);
    let hostname = arg(args, "hostname").map(str::to_string);
    cx.spawn(async move |this, cx| {
        // Enabling applies allow_tailnet_members even when Tailscale is on
        // already, so keep the server's setting unless asked to change it.
        let allow = match allow {
            Some(a) => a,
            None => control.call("server.status", json!({})).await.ok().and_then(|v| v["tailscale"]["allow_tailnet_members"].as_bool()).unwrap_or(false),
        };
        let mut payload = json!({"allow_tailnet_members": allow});
        if let Some(h) = hostname {
            payload["hostname"] = json!(h);
        }
        let result = control.call("server.tailscale.enable", payload).await;
        this.update(cx, |ws, cx| ws.store.update(cx, |s, cx| tailscale_done(s, "server.tailscale.enable", result, cx))).ok();
    })
    .detach();
    Ok(Value::Null)
}

fn tailscale_call(ws: &mut Workspace, method: &'static str, payload: Value, cx: &mut Context<Workspace>) -> Outcome {
    control_of(ws, cx)?;
    ws.store.update(cx, |s, cx| {
        // Set only once the call goes out, so a failed one leaves no
        // sign-in waiting to open a browser later.
        s.server.login_pending = method == "server.tailscale.login";
        s.call(method, payload, cx, move |s, result, cx| tailscale_done(s, method, result, cx))
    });
    Ok(Value::Null)
}

/// Whether Tailscale in `state` may still hand out a sign-in link.
fn wants_sign_in(state: &str) -> bool {
    !matches!(state, "running" | "disabled" | "needs_machine_approval")
}

/// Shows what a Tailscale call left Tailscale doing, and hands over the
/// sign-in link when there is one.
fn tailscale_done(store: &mut Store, method: &str, result: Result<Value, control::RpcError>, cx: &mut Context<Store>) {
    match result {
        Ok(v) => {
            let t = v["tailscale"].clone();
            store.server.tailscale = Some(t.clone());
            let link = s(&t, "auth_url").to_string();
            let state = s(&t, "state");
            match method {
                // Enabling a device that has never signed in also needs the
                // link, which may come only once the device has started.
                "server.tailscale.enable" => store.server.login_pending = matches!(state, "needs_login" | "starting"),
                "server.tailscale.disable" => store.server.login_pending = false,
                _ => {}
            }
            if !wants_sign_in(state) {
                store.server.login_pending = false;
            }
            if store.server.login_pending && !link.is_empty() {
                give_sign_in_link(store, &link);
            } else if store.server.login_pending && (method == "server.tailscale.login" || state == "needs_login") {
                show_notice(store, "Tailscale is making a sign-in link. It opens when ready.");
            } else {
                let name = s(&t["https"], "url");
                let name = if name.is_empty() { s(&t, "dns_name") } else { name };
                show_notice(store, format!("Tailscale is {}{}", tailscale_state(&t), if name.is_empty() { String::new() } else { format!(": {name}") }));
            }
        }
        Err(e) => {
            store.server.login_pending = false;
            store.notice = None;
            store.error = Some(format!("{method}: {}", e.message));
        }
    }
    cx.notify();
}

/// Copies the sign-in link, says so, and opens it in the browser.
fn give_sign_in_link(store: &mut Store, url: &str) {
    store.server.login_pending = false;
    store.pending_clipboard = Some(url.to_string());
    show_notice(store, format!("Copied Tailscale's sign-in link, and opening it: {url}"));
    open_url(url);
}

fn open_url(url: &str) {
    let url = url.to_string();
    // Wait on its own thread, so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        std::process::Command::new("xdg-open").arg(&url).status().ok();
    });
}

// Hosts.

/// Calls a method on the home server: on the window's connection while it
/// shows that server, else on a short connection of its own, since the host
/// list lives there.
fn home_call(ws: &Workspace, method: &'static str, payload: Value, cx: &mut App) -> Task<Result<Value, String>> {
    let store = ws.store.read(cx);
    let at_home = store.server.host.is_none();
    let control = store.control().filter(|_| at_home);
    let home = store.server.home.clone().unwrap_or_else(|| store.endpoint.clone());
    cx.spawn(async move |cx: &mut AsyncApp| {
        if let Some(control) = control {
            return control.call(method, payload).await.map_err(|e| format!("{method}: {}", e.message));
        }
        // No label: this short connection isn't one people should address.
        let hello = Hello { name: "OpenRex".into(), version: VERSION.into(), ..Default::default() };
        let conn = control::connect(&home, hello).await.map_err(|e| format!("reaching this machine's server at {home}: {e:#}"))?;
        let driver = cx.background_executor().spawn(conn.driver);
        let result = conn.control.call(method, payload).await.map_err(|e| format!("{method}: {}", e.message));
        conn.control.close();
        driver.await.ok();
        result
    })
}

/// Finds a host by ID, or by label ignoring case.
pub fn find_host<'a>(hosts: &'a [Host], query: &str) -> Option<&'a Host> {
    hosts.iter().find(|h| h.id == query).or_else(|| hosts.iter().find(|h| h.label.eq_ignore_ascii_case(query)))
}

/// The endpoints to try for a host's addresses, in order, without repeats.
/// A socket path counts only where it exists.
pub fn host_candidates(addresses: &[String], exists: impl Fn(&Path) -> bool) -> Vec<Endpoint> {
    let mut out: Vec<Endpoint> = Vec::new();
    for e in addresses.iter().filter_map(|a| Endpoint::candidates(a).ok()).flatten() {
        if matches!(&e, Endpoint::Unix(p) if !exists(p)) || out.contains(&e) {
            continue;
        }
        out.push(e);
    }
    out
}

/// Words that mean the home server in `client.host.switch`.
fn means_home(query: &str) -> bool {
    ["local", "home", "this server", "this machine", "localhost"].iter().any(|w| w.eq_ignore_ascii_case(query))
}

/// Where `client.host.switch` with `query` goes: the home server, a host by
/// ID or label, or else an address.
pub fn resolve(query: &str, hosts: &[Host], home: &Endpoint) -> Result<Destination, String> {
    let to_home = || Destination { label: None, candidates: vec![home.clone()] };
    if means_home(query) {
        return Ok(to_home());
    }
    if let Some(h) = find_host(hosts, query) {
        let candidates = host_candidates(&h.endpoints, Path::exists);
        if candidates.is_empty() {
            return Err(format!("host {} has no address this device can use", h.label));
        }
        return Ok(Destination { label: Some(h.label.clone()), candidates });
    }
    match Endpoint::candidates(query) {
        Ok(c) if c.contains(home) => Ok(to_home()),
        Ok(c) => Ok(Destination { label: Some(query.to_string()), candidates: c }),
        Err(_) => Err(format!("no host is called {query}")),
    }
}

/// Tries each endpoint in turn and gives the first that answers.
async fn first_reachable(candidates: Vec<Endpoint>) -> Result<Endpoint, String> {
    let mut why = "it has no address to try".to_string();
    for ep in candidates {
        let named = ep.to_string();
        let probe = async { ep.probe().await.map_err(|e| format!("{e:#}")) };
        let timeout = async {
            smol::Timer::after(PROBE_TIMEOUT).await;
            Err(format!("{named} didn't answer in time"))
        };
        match smol::future::or(probe, timeout).await {
            Ok(()) => return Ok(ep.clone()),
            Err(e) => why = e,
        }
    }
    Err(why)
}

/// Moves the window to another server, once one of its endpoints answers.
/// The current connection stays until then, so a host that can't be reached
/// leaves the window as it was.
pub fn switch(store: &mut Store, dest: Destination, cx: &mut Context<Store>) {
    let name = dest.label.clone().unwrap_or_else(|| "this machine's server".into());
    if dest.label == store.server.host && store.status == Status::Connected {
        show_notice(store, format!("Already connected to {name}"));
        cx.notify();
        return;
    }
    show_notice(store, format!("Connecting to {name}…"));
    let probe = cx.background_executor().spawn(first_reachable(dest.candidates));
    cx.spawn(async move |this, cx| {
        let found = probe.await;
        this.update(cx, |s, cx| match found {
            Ok(endpoint) => reconnect(s, endpoint, dest.label, cx),
            Err(why) => {
                s.notice = None;
                s.error = Some(format!("Couldn't reach {name}: {why}"));
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
    cx.notify();
}

/// Closes the connection cleanly, forgets the old server's sessions, and
/// connects to `endpoint`.
fn reconnect(store: &mut Store, endpoint: Endpoint, host: Option<String>, cx: &mut Context<Store>) {
    if store.server.home.is_none() {
        store.server.home = Some(store.endpoint.clone());
    }
    let old = store.connection.take();
    if let Some(control) = store.control() {
        control.close();
    }
    store.server.host = host;
    store.server.tailscale = None;
    store.endpoint = endpoint;
    store.sessions.clear();
    store.current = None;
    store.views.clear();
    store.panes.clear();
    store.status = Status::Connecting;
    cx.spawn(async move |this, cx| {
        // Let the old connection finish closing, so the server sees it leave.
        if let Some(old) = old {
            let give_up = async {
                smol::Timer::after(Duration::from_secs(2)).await;
            };
            smol::future::or(old, give_up).await;
        }
        // The new connection reads the host list once it is up.
        this.update(cx, |s, cx| {
            s.connect(cx);
            s.notice = None;
        })
        .ok();
    })
    .detach();
    cx.notify();
}

fn host_switch(ws: &mut Workspace, host: Option<&str>, cx: &mut Context<Workspace>) -> Outcome {
    let home = {
        let store = ws.store.read(cx);
        store.server.home.clone().unwrap_or_else(|| store.endpoint.clone())
    };
    if let Some(query) = host.map(str::to_string) {
        // Read the list first, so a host added elsewhere a moment ago works.
        let list = home_call(ws, "hosts.list", json!({}), cx);
        cx.spawn(async move |this, cx| {
            let list = list.await;
            this.update(cx, |ws, cx| {
                ws.store.update(cx, |s, cx| {
                    if let Ok(v) = &list {
                        s.server.hosts = parse_hosts(v);
                        s.server.hosts_hash = v["hash"].as_str().unwrap_or("").to_string();
                    }
                    match resolve(&query, &s.server.hosts, &home) {
                        Ok(dest) => switch(s, dest, cx),
                        Err(e) => {
                            s.notice = None;
                            s.error = Some(e);
                            cx.notify();
                        }
                    }
                });
            })
            .ok();
        })
        .detach();
        return Ok(Value::Null);
    }
    const TITLE: &str = "Switch Host";
    let items = move |store: &Store| {
        let current = store.server.host.clone();
        let mut items = vec![Item::new("This server", "client.host.switch", json!({"host": home.to_string()}))
            .detail(home.to_string())
            .hint(if current.is_none() { "current" } else { "" })
            .keywords("local home this machine")];
        for h in &store.server.hosts {
            let here = current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&h.label));
            items.push(Item::new(h.label.clone(), "client.host.switch", json!({"host": h.id})).detail(h.endpoints.join(", ")).hint(if here { "current" } else { "" }));
        }
        items
    };
    ws.palette = Some(Palette::pick(TITLE, items(ws.store.read(cx))).empty("No host matches"));
    refresh_hosts_palette(ws, TITLE, items, cx);
    Ok(Value::Null)
}

/// Reads the host list again and, if the palette still shows `title`,
/// rebuilds its rows from the new list.
fn refresh_hosts_palette(ws: &mut Workspace, title: &'static str, items: impl Fn(&Store) -> Vec<Item> + 'static, cx: &mut Context<Workspace>) {
    let list = home_call(ws, "hosts.list", json!({}), cx);
    cx.spawn(async move |this, cx| {
        let list = list.await;
        this.update(cx, |ws, cx| {
            match list {
                Ok(v) => ws.store.update(cx, |s, _| {
                    s.server.hosts = parse_hosts(&v);
                    s.server.hosts_hash = v["hash"].as_str().unwrap_or("").to_string();
                }),
                Err(e) => show_error(ws, e, cx),
            }
            let fresh = items(ws.store.read(cx));
            if let Some(p) = &mut ws.palette {
                if p.title == title {
                    p.items = fresh;
                    p.selected = p.selected.min(p.items.len().saturating_sub(1));
                }
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// A label for a host at `address`: the first part of its name, as
/// `studio` for `studio.tail1234.ts.net`.
pub fn suggested_label(address: &str) -> String {
    let Some(first) = Endpoint::candidates(address).ok().and_then(|c| c.into_iter().next()) else { return String::new() };
    match first {
        Endpoint::Tcp { host, .. } | Endpoint::Tls { host, .. } => {
            if host.parse::<std::net::IpAddr>().is_ok() {
                host
            } else {
                host.split('.').next().unwrap_or(&host).to_string()
            }
        }
        Endpoint::Unix(p) => p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

fn host_add(ws: &mut Workspace, address: Option<&str>, label: Option<&str>, cx: &mut Context<Workspace>) -> Outcome {
    let Some(address) = address else {
        ws.palette = Some(
            Palette::input("Add Remote Host", "client.host.add", json!({}), "address", "")
                .placeholder("Its address, such as studio.tail1234.ts.net or box:7297"),
        );
        return Ok(Value::Null);
    };
    match Endpoint::candidates(address) {
        Err(e) => return fail("bad_request", format!("{e:#}")),
        // The server refuses them: a socket path works only on its own machine.
        Ok(c) if matches!(c.first(), Some(Endpoint::Unix(_))) => {
            return fail("bad_request", "a host needs a network address, not a socket path");
        }
        Ok(_) => {}
    }
    let Some(label) = label else {
        ws.palette = Some(
            Palette::input("Name the Host", "client.host.add", json!({"address": address}), "label", &suggested_label(address))
                .placeholder("A name to connect to it by"),
        );
        return Ok(Value::Null);
    };
    let from = std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
    let mut host = json!({"label": label, "endpoints": [address]});
    if !from.is_empty() {
        host["added_from"] = json!(format!("OpenRex on {from}"));
    }
    let put = home_call(ws, "hosts.put", json!({"host": host}), cx);
    let label = label.to_string();
    cx.spawn(async move |this, cx| {
        let result = put.await;
        this.update(cx, |ws, cx| match result {
            Ok(_) => ws.store.update(cx, |s, cx| {
                show_notice(s, format!("Added host {label}"));
                if s.server.host.is_none() {
                    fetch_hosts(s, cx);
                }
                cx.notify();
            }),
            Err(e) => show_error(ws, e, cx),
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

fn host_remove(ws: &mut Workspace, host: Option<&str>, cx: &mut Context<Workspace>) -> Outcome {
    let Some(query) = host.map(str::to_string) else {
        const TITLE: &str = "Remove Host";
        let items = |store: &Store| -> Vec<Item> {
            store
                .server
                .hosts
                .iter()
                .map(|h| Item::new(h.label.clone(), "client.host.remove", json!({"host": h.id})).detail(h.endpoints.join(", ")))
                .collect()
        };
        ws.palette = Some(Palette::pick(TITLE, items(ws.store.read(cx))).empty("There are no hosts"));
        refresh_hosts_palette(ws, TITLE, items, cx);
        return Ok(Value::Null);
    };
    let list = home_call(ws, "hosts.list", json!({}), cx);
    cx.spawn(async move |this, cx| {
        let listed = list.await;
        let Ok(hosts) = this.update(cx, |ws, cx| match listed {
            Ok(v) => parse_hosts(&v),
            Err(_) => ws.store.read(cx).server.hosts.clone(),
        }) else {
            return;
        };
        let Some(found) = find_host(&hosts, &query).cloned() else {
            this.update(cx, |ws, cx| show_error(ws, format!("no host is called {query}"), cx)).ok();
            return;
        };
        let Ok(remove) = this.update(cx, |ws, cx| home_call(ws, "hosts.remove", json!({"id": found.id}), cx)) else { return };
        let result = remove.await;
        this.update(cx, |ws, cx| match result {
            Ok(_) => ws.store.update(cx, |s, cx| {
                s.server.hosts.retain(|h| h.id != found.id);
                show_notice(s, format!("Removed host {}", found.label));
                cx.notify();
            }),
            Err(e) => show_error(ws, e, cx),
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// The text the status bar shows for the connection: the endpoint, and the
/// host's name when the window shows a host.
pub fn connection_label(store: &Store, label: String) -> String {
    match &store.server.host {
        Some(host) => format!("{host} · {label}"),
        None => label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<Host> {
        parse_hosts(&json!({"hosts": [
            {"id": "host:1", "label": "studio", "endpoints": ["studio.tail1234.ts.net", "192.168.1.20:7297"]},
            {"id": "host:2", "label": "attic", "endpoints": ["unix:///nonexistent/rex.sock", "http://attic:7297"]},
            {"label": "no id", "endpoints": []},
        ], "hash": "h"}))
    }

    #[test]
    fn reads_hosts_and_finds_them_by_id_or_label() {
        let hosts = hosts();
        assert_eq!(hosts.len(), 2);
        assert_eq!(find_host(&hosts, "host:2").map(|h| h.label.as_str()), Some("attic"));
        assert_eq!(find_host(&hosts, "STUDIO").map(|h| h.id.as_str()), Some("host:1"));
        assert!(find_host(&hosts, "garage").is_none());
    }

    #[test]
    fn resolves_switch_targets() {
        let home = Endpoint::Unix("/run/user/1000/rex/server.sock".into());
        let hosts = hosts();
        assert_eq!(resolve("local", &hosts, &home).unwrap(), Destination { label: None, candidates: vec![home.clone()] });
        assert_eq!(resolve(&home.to_string(), &hosts, &home).unwrap().label, None);
        let attic = resolve("attic", &hosts, &home).unwrap();
        // The socket path doesn't exist here, so only the URL is tried.
        assert_eq!(attic.candidates, vec![Endpoint::Tcp { host: "attic".into(), port: 7297 }]);
        assert_eq!(attic.label.as_deref(), Some("attic"));
        let studio = resolve("studio", &hosts, &home).unwrap();
        assert_eq!(studio.candidates.len(), 5);
        assert_eq!(studio.candidates[0], Endpoint::Tls { host: "studio.tail1234.ts.net".into(), port: 443 });
        let raw = resolve("box:7300", &hosts, &home).unwrap();
        assert_eq!(raw.label.as_deref(), Some("box:7300"));
        assert!(resolve("not an address", &hosts, &home).is_err());
    }

    #[test]
    fn host_candidates_skip_missing_sockets_and_repeats() {
        let addrs = vec!["unix:///here.sock".to_string(), "unix:///gone.sock".into(), "box:1".into(), "https://box:1".into()];
        let found = host_candidates(&addrs, |p| p == Path::new("/here.sock"));
        assert_eq!(
            found,
            vec![
                Endpoint::Unix("/here.sock".into()),
                Endpoint::Tls { host: "box".into(), port: 1 },
                Endpoint::Tcp { host: "box".into(), port: 1 },
            ]
        );
    }

    #[test]
    fn suggests_labels_from_addresses() {
        assert_eq!(suggested_label("studio.tail1234.ts.net"), "studio");
        assert_eq!(suggested_label("https://attic.example.com:8443"), "attic");
        assert_eq!(suggested_label("192.168.1.20:7297"), "192.168.1.20");
        assert_eq!(suggested_label(""), "");
    }

    #[test]
    fn checks_client_labels() {
        assert_eq!(check_label("  studio "), Ok("studio".into()));
        assert!(check_label("   ").is_err());
        assert!(check_label(&"x".repeat(65)).is_err());
        assert!(check_label("a\tb").is_err());
    }

    #[test]
    fn shortens_times_and_describes_versions() {
        assert_eq!(short_time("2026-11-13T09:12:39.642309191Z"), "2026-11-13T09:12:39Z");
        assert_eq!(short_time("2026-11-13T09:12:39.5+01:00"), "2026-11-13T09:12:39+01:00");
        assert_eq!(short_time("2026-11-13T09:12:39Z"), "2026-11-13T09:12:39Z");
        let v = json!({"rex_version": "0.0.1794546118-g228358", "platform": "linux_amd64", "commit": "22835813567a08307c", "dirty": false});
        assert_eq!(version_line(&v), "0.0.1794546118-g228358 (linux_amd64, commit 2283581)");
        assert_eq!(version_line(&json!({"rex_version": "1.0", "dirty": true})), "1.0 (dirty)");
    }

    #[test]
    fn describes_tailscale() {
        assert_eq!(tailscale_state(&json!({"state": "disabled"})), "off");
        assert_eq!(tailscale_state(&json!({"state": "running", "https": {"state": "available"}})), "on");
        assert_eq!(tailscale_state(&json!({"state": "running"})), "on, HTTPS pending");
        assert_eq!(tailscale_state(&json!({"state": "something new"})), "starting");
        let row = tailscale_row(Some(&json!({"state": "running", "dns_name": "rex.ts.net", "account": {"login_name": "a@b.c"}, "https": {"state": "available", "url": "https://rex.ts.net"}})));
        assert_eq!(row.label, "Tailscale: on");
        assert_eq!(row.detail, "https://rex.ts.net, a@b.c");
        assert_eq!(tailscale_row(None).label, "Tailscale: not available");
    }

    fn info() -> ServerInfo {
        ServerInfo {
            version: Some(Ok(json!({"rex_version": "0.9", "platform": "linux_amd64", "api_revision": 13}))),
            status: Some(Ok(json!({
                "pid": 7, "instance_id": "i-1", "host_id": "h-1", "started_at": "2026-11-13T09:00:00.5Z", "exit_empty": false,
                "session_count": 1, "tailscale": {"state": "disabled", "allow_tailnet_members": false},
                "config": {"generation": 2, "hash": "abcdef0123456789"},
                "listeners": [{"endpoint": "unix:///s.sock", "transport": "unix", "admission": "peer_credentials", "primary": true}],
            }))),
            whoami: Some(Ok(json!({"principal": {"kind": "peer", "transport": "unix", "display": "uid 1000", "remote_addr": "@"}, "account": "goody"}))),
            clients: Some(Ok(json!({"clients": [
                {"client_id": "client:me", "info": {"kind": "app", "platform": "linux", "name": "OpenRex", "version": "0.1.0", "label": "desk"},
                 "principal": {"display": "uid 1000", "transport": "unix"}, "connected_at": "2026-11-13T10:00:00.1Z", "session_ids": ["s1"]},
                {"client_id": "client:old", "principal": {"display": "uid 1000", "transport": "unix"}, "connection_state": "lost"},
            ]}))),
        }
    }

    fn local() -> LocalInfo {
        LocalInfo {
            endpoint: "unix:///s.sock".into(),
            client_id: Some("client:me".into()),
            keymap_path: "/home/u/.config/rex/init.lua".into(),
            keymap_diagnostics: vec!["warning init.lua:3: unknown action".into()],
            ..Default::default()
        }
    }

    #[test]
    fn reports_hold_their_sections() {
        let all = report_rows(&info(), &local(), Report::All);
        let labels: Vec<&str> = all.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"Rex server 0.9 (linux_amd64)"));
        assert!(labels.contains(&"1 session"));
        assert!(labels.contains(&"Config loaded 2 times"));
        assert!(labels.contains(&"Listening on unix:///s.sock"));
        assert!(labels.contains(&"Tailscale: off"));
        assert!(labels.contains(&"You are uid 1000"));
        assert!(labels.contains(&"/home/u/.config/rex/init.lua"));
        assert!(labels.contains(&"warning init.lua:3: unknown action"));
        let me = all.iter().find(|r| r.section == "client" && r.label == "OpenRex 0.1.0").unwrap();
        assert_eq!(me.mark, "this client");
        assert_eq!(me.detail, "label desk, app on linux, uid 1000 over unix, 1 session, connected 2026-11-13T10:00:00Z, client:me");
        assert_eq!(all.iter().find(|r| r.label == "client:old").unwrap().mark, "lost");
        let you = all.iter().find(|r| r.section == "you").unwrap();
        assert_eq!(you.text(), "You are uid 1000: over unix, the server runs as goody");

        let status = report_rows(&info(), &local(), Report::Status);
        assert!(status.iter().all(|r| r.section != "client" && r.section != "keymap"));
        let clients = report_rows(&info(), &local(), Report::Clients);
        assert!(clients.iter().all(|r| r.section == "client"));
        assert_eq!(clients.len(), 2);

        let failed = ServerInfo { version: Some(Err("boom".into())), ..Default::default() };
        let rows = report_rows(&failed, &LocalInfo::default(), Report::Status);
        assert_eq!(rows[0].text(), "Rex server version: unknown: boom");
    }

    #[test]
    fn debug_reports_are_plain_text() {
        let d = DebugState {
            endpoint: "unix:///s.sock".into(),
            status: "connected".into(),
            client_id: "client:me".into(),
            label: Some("desk".into()),
            os: "Arch Linux (x86_64)".into(),
            sessions: 2,
            tabs: 3,
            panes: 4,
            open_panes: 4,
            keymap_path: "/x/init.lua".into(),
            keymap_exists: true,
            bindings: 50,
            diagnostics: 1,
            ..Default::default()
        };
        let v = json!({"rex_version": "0.9", "platform": "linux_amd64", "api_revision": 13});
        let report = debug_report(&d, Some(&v), Some(&json!({"session_count": 2, "tailscale": {"state": "disabled"}})));
        assert!(report.starts_with(&format!("OpenRex {VERSION}\nRex server 0.9 (linux_amd64)\n")));
        assert!(report.contains("Host: this machine's server\n"));
        assert!(report.contains("Client: client:me (label desk)\n"));
        assert!(report.contains("Sessions: 2 (2 on the server)\n"));
        assert!(report.contains("Current session: 3 tabs, 4 panes\n"));
        assert!(report.contains("Tailscale: off\n"));
        assert!(report.contains("Keymap: /x/init.lua (exists), 50 bindings, 1 diagnostics\n"));
        assert!(debug_report(&DebugState::default(), None, None).contains("Rex server: unknown"));
    }

    #[test]
    fn reads_os_release() {
        assert_eq!(os_pretty_name("NAME=Arch\nPRETTY_NAME=\"Arch Linux\"\n").as_deref(), Some("Arch Linux"));
        assert_eq!(os_pretty_name("NAME=x"), None);
    }
}
