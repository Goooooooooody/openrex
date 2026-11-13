//! What programs are doing: program status (OSC 7501), progress reports, bells, and exits, shown on sessions and tabs; pane.view_activity and client.sidebar.activity.toggle.
//!
//! The store keeps a [`Tracker`] fed by every terminal's block events. The
//! client bootstraps with `event_scope` `all`, so events arrive for sessions
//! it never attached to, and the tracker also learns every terminal's title,
//! which is what lets a tab that was never shown carry its real title.
//!
//! Records are the server's. What the client adds is whether the user has
//! seen them: a `done` report, a bell, or a clean exit stops being shown once
//! its pane has been on screen in the active window.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, ClickEvent, Context, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, relative, rgb,
};
use openrex_proto::types::{SessionView, Window as RexWindow};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target};
use crate::palette::{Item, Palette};
use crate::store::{Store, active_window};
use crate::workspace::Workspace;

const TERMINAL: &str = "com.superlogical.terminal";

/// How long a bell flashes the border of a pane on screen.
const FLASH: Duration = Duration::from_millis(450);

/// A bell that rang this recently, in a pane that is on screen, flashes it.
/// An older one, as when the window was in the background, only clears.
const FRESH_BELL: Duration = Duration::from_millis(1500);

/// The longest message shown in a list, in characters.
const MESSAGE_CHARS: usize = 120;

/// How much something wants the user, least first, so the most urgent of a
/// set is its maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Idle,
    Bell,
    Done,
    Working,
    Error,
    Blocked,
}

impl Level {
    /// The name of a group of this level in lists.
    pub fn group(self) -> &'static str {
        match self {
            Level::Blocked => "Needs you",
            Level::Error => "Failed",
            Level::Working => "Working",
            Level::Done => "Done",
            Level::Bell => "Bell",
            Level::Idle => "Idle",
        }
    }

    /// One word for the level, for the palette.
    pub fn word(self) -> &'static str {
        match self {
            Level::Blocked => "blocked",
            Level::Error => "error",
            Level::Working => "working",
            Level::Done => "done",
            Level::Bell => "bell",
            Level::Idle => "idle",
        }
    }
}

/// One program status record, as `program_status` and
/// `program_status_changed` describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    /// Empty for the root record.
    pub id: String,
    pub state: String,
    pub kind: String,
    pub progress: Option<u8>,
    /// The program, from `effective_app` or else `app`.
    pub app: String,
    pub title: String,
    pub msg: String,
    pub updated_at: String,
    /// Whether the user has seen this report, for `done`.
    pub seen: bool,
}

impl Record {
    /// Reads a record. `id` overrides the record's own, which the server
    /// leaves out for the root record. `None` without a state.
    pub fn from_json(id: Option<&str>, v: &Value) -> Option<Record> {
        let state = v["state"].as_str()?.to_string();
        let text = |k: &str| v[k].as_str().map(sanitize).unwrap_or_default();
        let app = v["effective_app"].as_str().filter(|a| !a.is_empty()).or(v["app"].as_str()).map(sanitize).unwrap_or_default();
        Some(Record {
            id: id.or(v["id"].as_str()).unwrap_or("").to_string(),
            state,
            kind: text("kind"),
            progress: v["progress"].as_u64().map(|p| p.min(100) as u8),
            app,
            title: text("title"),
            msg: text("msg"),
            updated_at: text("updated_at"),
            seen: false,
        })
    }

    /// The level the record shows at. A state the client doesn't know is
    /// idle, as the protocol asks, and a `done` the user saw is too.
    pub fn level(&self) -> Level {
        match self.state.as_str() {
            "blocked" => Level::Blocked,
            "error" => Level::Error,
            "working" => Level::Working,
            "done" if !self.seen => Level::Done,
            _ => Level::Idle,
        }
    }

    fn message(&self) -> String {
        if !self.msg.is_empty() {
            return self.msg.clone();
        }
        match self.level() {
            Level::Blocked => {
                let why = match self.kind.as_str() {
                    "permission" => "Waiting for permission",
                    "question" => "Asking a question",
                    "auth" => "Waiting for a sign-in",
                    _ => "Waiting for you",
                };
                why.to_string()
            }
            Level::Error => "Failed".to_string(),
            Level::Working => working_text(self.progress),
            Level::Done => "Finished".to_string(),
            _ => String::new(),
        }
    }
}

fn working_text(progress: Option<u8>) -> String {
    match progress {
        Some(p) => format!("Working, {p}%"),
        None => "Working".to_string(),
    }
}

/// A progress report (OSC 9;4), which says only busy, a percentage, or error.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub state: String,
    pub progress: Option<u8>,
}

impl Progress {
    /// Reads a `progress_report`. `Ok(None)` for `remove`, and an error for a
    /// state the client doesn't know, which leaves the last report standing.
    pub fn from_report(p: &Value) -> Result<Option<Progress>, ()> {
        let state = p["state"].as_str().unwrap_or("");
        match state {
            "set" | "indeterminate" | "pause" | "error" => Ok(Some(Progress {
                state: state.to_string(),
                progress: if state == "indeterminate" { None } else { p["progress"].as_u64().map(|v| v.min(100) as u8) },
            })),
            "remove" => Ok(None),
            _ => Err(()),
        }
    }

    pub fn level(&self) -> Level {
        if self.state == "error" { Level::Error } else { Level::Working }
    }

    fn message(&self) -> String {
        match self.state.as_str() {
            "error" => "Reported an error".to_string(),
            "pause" => match self.progress {
                Some(p) => format!("Paused, {p}%"),
                None => "Paused".to_string(),
            },
            _ => working_text(self.progress),
        }
    }
}

/// What the client knows about one terminal.
#[derive(Clone, Debug, Default)]
pub struct BlockState {
    pub session_id: String,
    pub title: String,
    /// The title came from `title_changed`, so a slower fetch mustn't
    /// replace it.
    title_from_event: bool,
    /// By record ID, which sorts the root first and parents before children.
    pub records: BTreeMap<String, Record>,
    /// Record IDs that events changed, which a slower fetch mustn't undo.
    touched: HashSet<String>,
    pub progress: Option<Progress>,
    /// A bell rang that the user hasn't seen.
    pub bell: bool,
    bell_at: Option<Instant>,
    /// The program's exit code, once it exited.
    pub exited: Option<i64>,
    exit_seen: bool,
    /// When something last happened, to list the newest first.
    tick: u64,
}

impl BlockState {
    /// What the block has to show, most urgent first.
    fn entries(&self, block_id: &str) -> Vec<Entry> {
        let base = Entry {
            session_id: self.session_id.clone(),
            block_id: block_id.to_string(),
            level: Level::Idle,
            headline: if self.title.is_empty() { "Terminal".to_string() } else { self.title.clone() },
            message: String::new(),
            progress: None,
            tick: self.tick,
        };
        let mut out = Vec::new();
        for r in self.records.values() {
            let level = r.level();
            if level == Level::Idle {
                continue;
            }
            let headline = match (r.app.is_empty(), r.title.is_empty()) {
                (false, false) => format!("{} · {}", r.app, r.title),
                (false, true) => r.app.clone(),
                (true, false) => r.title.clone(),
                (true, true) => base.headline.clone(),
            };
            let progress = if matches!(level, Level::Working | Level::Blocked) { r.progress } else { None };
            out.push(Entry { level, headline, message: r.message(), progress, ..base.clone() });
        }
        // A program that reports its status says more than its progress
        // reports, so those count only without records, as the protocol
        // suggests.
        if self.records.is_empty() {
            if let Some(p) = &self.progress {
                out.push(Entry { level: p.level(), message: p.message(), progress: p.progress, ..base.clone() });
            }
        }
        if self.bell {
            out.push(Entry { level: Level::Bell, message: "Rang the bell".into(), ..base.clone() });
        }
        match self.exited {
            Some(0) if !self.exit_seen => out.push(Entry { level: Level::Done, message: "Exited".into(), ..base.clone() }),
            // The server says -1 when a signal ended the program.
            Some(-1) => out.push(Entry { level: Level::Error, message: "Ended by a signal".into(), ..base.clone() }),
            Some(code) if code != 0 => out.push(Entry { level: Level::Error, message: format!("Exited with code {code}"), ..base.clone() }),
            _ => {}
        }
        out.sort_by(|a, b| b.level.cmp(&a.level));
        out
    }
}

/// One thing a terminal is doing that the user may want to know about.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub session_id: String,
    pub block_id: String,
    pub level: Level,
    /// The program and the record's title, or the terminal's title.
    pub headline: String,
    pub message: String,
    pub progress: Option<u8>,
    tick: u64,
}

/// The most urgent level of a set of terminals, as a tab or session shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub level: Level,
    /// For working or blocked, the progress of the first one that has one.
    pub progress: Option<u8>,
    /// How many things are at `level`.
    pub count: usize,
}

/// Every terminal's title and activity, by block ID.
#[derive(Debug, Default)]
pub struct Tracker {
    blocks: HashMap<String, BlockState>,
    /// Blocks whose title and status were asked for.
    fetched: HashSet<String>,
    tick: u64,
}

impl Tracker {
    fn block(&mut self, session_id: &str, block_id: &str) -> &mut BlockState {
        let b = self.blocks.entry(block_id.to_string()).or_default();
        if !session_id.is_empty() {
            b.session_id = session_id.to_string();
        }
        b
    }

    fn bump(&mut self, block_id: &str) {
        self.tick += 1;
        if let Some(b) = self.blocks.get_mut(block_id) {
            b.tick = self.tick;
        }
    }

    /// The terminal's title, as its program last set it.
    pub fn title(&self, block_id: &str) -> Option<&str> {
        self.blocks.get(block_id).map(|b| b.title.as_str()).filter(|t| !t.is_empty())
    }

    pub fn get(&self, block_id: &str) -> Option<&BlockState> {
        self.blocks.get(block_id)
    }

    /// Applies one of a terminal's block events.
    pub fn on_event(&mut self, session_id: &str, block_id: &str, name: &str, p: &Value) {
        match name {
            "title_changed" => {
                let b = self.block(session_id, block_id);
                b.title = p["title"].as_str().map(sanitize).unwrap_or_default();
                b.title_from_event = true;
            }
            "program_status_changed" => {
                let id = p["id"].as_str().unwrap_or("");
                let Some(record) = Record::from_json(Some(id), &p["record"]) else { return };
                let b = self.block(session_id, block_id);
                b.touched.insert(id.to_string());
                b.records.insert(id.to_string(), record);
                self.bump(block_id);
            }
            "program_status_removed" => {
                let b = self.block(session_id, block_id);
                for id in p["ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    b.records.remove(id);
                    b.touched.insert(id.to_string());
                }
            }
            "progress_report" => {
                if let Ok(progress) = Progress::from_report(p) {
                    self.block(session_id, block_id).progress = progress;
                    self.bump(block_id);
                }
            }
            "bell" => {
                let b = self.block(session_id, block_id);
                b.bell = true;
                b.bell_at = Some(Instant::now());
                self.bump(block_id);
            }
            "child_exited" => {
                let b = self.block(session_id, block_id);
                b.exited = Some(p["exit_code"].as_i64().unwrap_or(-1));
                b.exit_seen = false;
                b.progress = None;
                self.bump(block_id);
            }
            _ => {}
        }
    }

    /// Applies a fetched title, unless an event already said one.
    pub fn fetched_title(&mut self, session_id: &str, block_id: &str, title: &str) {
        let b = self.block(session_id, block_id);
        if !b.title_from_event {
            b.title = sanitize(title);
        }
    }

    /// Applies a fetched `program_status` result, keeping what events have
    /// changed since.
    pub fn fetched_status(&mut self, session_id: &str, block_id: &str, result: &Value) {
        let b = self.block(session_id, block_id);
        let mut any = false;
        for r in result["records"].as_array().into_iter().flatten() {
            let Some(record) = Record::from_json(None, r) else { continue };
            if !b.touched.contains(&record.id) {
                b.records.insert(record.id.clone(), record);
                any = true;
            }
        }
        if any {
            self.bump(block_id);
        }
    }

    pub fn forget(&mut self, block_id: &str) {
        self.blocks.remove(block_id);
        self.fetched.remove(block_id);
    }

    pub fn forget_session(&mut self, session_id: &str) {
        let gone: Vec<String> = self.blocks.iter().filter(|(_, b)| b.session_id == session_id).map(|(id, _)| id.clone()).collect();
        for id in gone {
            self.forget(&id);
        }
    }

    /// Marks what a block has to show as seen, because it is on screen.
    /// Says whether anything changed, and whether a bell rang just now.
    pub fn see(&mut self, block_id: &str, now: Instant) -> (bool, bool) {
        let Some(b) = self.blocks.get_mut(block_id) else { return (false, false) };
        let mut changed = false;
        for r in b.records.values_mut() {
            if r.state == "done" && !r.seen {
                r.seen = true;
                changed = true;
            }
        }
        let mut rang = false;
        if b.bell {
            b.bell = false;
            changed = true;
            rang = b.bell_at.is_some_and(|t| now.saturating_duration_since(t) < FRESH_BELL);
        }
        if b.exited.is_some() && !b.exit_seen {
            b.exit_seen = true;
            changed = true;
        }
        (changed, rang)
    }

    /// What one block has to show, most urgent first.
    pub fn block_entries(&self, block_id: &str) -> Vec<Entry> {
        self.blocks.get(block_id).map(|b| b.entries(block_id)).unwrap_or_default()
    }

    /// Everything every terminal has to show: most urgent first, then the
    /// newest.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = self.blocks.iter().flat_map(|(id, b)| b.entries(id)).collect();
        out.sort_by(|a, b| b.level.cmp(&a.level).then(b.tick.cmp(&a.tick)).then(a.block_id.cmp(&b.block_id)));
        out
    }

    /// The most urgent of what some blocks have to show, or `None` if they
    /// are all idle.
    pub fn summary<'a>(&self, block_ids: impl IntoIterator<Item = &'a str>) -> Option<Summary> {
        summarize(block_ids.into_iter().flat_map(|id| self.block_entries(id)))
    }

    /// The most urgent of what a session's blocks have to show.
    pub fn session_summary(&self, session_id: &str) -> Option<Summary> {
        summarize(self.blocks.iter().filter(|(_, b)| b.session_id == session_id).flat_map(|(id, b)| b.entries(id)))
    }
}

fn summarize(entries: impl IntoIterator<Item = Entry>) -> Option<Summary> {
    let mut best: Option<Summary> = None;
    for e in entries {
        let level = best.as_ref().map(|s| s.level);
        if level.is_none_or(|l| e.level > l) {
            best = Some(Summary { level: e.level, progress: e.progress, count: 1 });
        } else if let Some(s) = best.as_mut().filter(|s| s.level == e.level) {
            s.count += 1;
            s.progress = s.progress.or(e.progress);
        }
    }
    best.filter(|s| s.level != Level::Idle)
}

/// Removes what mustn't reach the screen from a program's text: control
/// characters, and the invisible marks that reorder text, which could make
/// one program's message look like another's.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            !c.is_control()
                && !matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}')
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Shortens text to `max` characters, marking the cut.
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Every block of a window.
pub fn window_blocks(win: &RexWindow) -> impl Iterator<Item = &str> {
    win.layers.iter().flat_map(|l| &l.blocks).filter_map(|b| b.block_id.as_deref())
}

/// The blocks a window shows: only the zoomed one while one is zoomed, and
/// none of a minimized floating layer.
pub fn visible_blocks(win: &RexWindow) -> Vec<String> {
    if let Some(zoomed) = &win.zoomed_block_id {
        return vec![zoomed.clone()];
    }
    win.layers.iter().filter(|l| !l.is_minimized()).flat_map(|l| &l.blocks).filter_map(|b| b.block_id.clone()).collect()
}

// The store's side: learning about blocks it hasn't heard from.

/// Starts over after (re)connecting, and asks every session for its
/// terminals, so sessions and tabs that were never shown have their titles
/// and status.
pub fn bootstrap(store: &mut Store, cx: &mut Context<Store>) {
    store.activity = Tracker::default();
    let sessions: Vec<String> = store.sessions.iter().map(|s| s.id.clone()).collect();
    for session_id in sessions {
        let sid = session_id.clone();
        store.call("session.list_blocks", json!({"session_id": session_id}), cx, move |store, result, cx| {
            let Ok(v) = result else { return };
            for b in v["blocks"].as_array().into_iter().flatten() {
                if b["creator_name"].as_str() != Some(TERMINAL) {
                    continue;
                }
                if let Some(block_id) = b["block_id"].as_str() {
                    learn(store, &sid, block_id, cx);
                }
            }
        });
    }
}

/// Learns the terminals of a session's view, as it is attached or changes.
pub fn on_view(store: &mut Store, view: &SessionView, cx: &mut Context<Store>) {
    let Some(session_id) = view.session_id.clone() else { return };
    let blocks: Vec<String> = view
        .windows
        .iter()
        .flat_map(|w| &w.layers)
        .flat_map(|l| &l.blocks)
        .filter(|b| b.creator_name == TERMINAL)
        .filter_map(|b| b.block_id.clone())
        .collect();
    for block_id in blocks {
        learn(store, &session_id, &block_id, cx);
    }
}

/// Asks for a terminal's title and status, once.
fn learn(store: &mut Store, session_id: &str, block_id: &str, cx: &mut Context<Store>) {
    store.activity.block(session_id, block_id);
    if !store.activity.fetched.insert(block_id.to_string()) {
        return;
    }
    let payload = json!({"session_id": session_id, "block_id": block_id, "args": {}});
    let (sid, bid) = (session_id.to_string(), block_id.to_string());
    store.call("com.superlogical.terminal.title", payload.clone(), cx, move |store, result, _| {
        if let Some(title) = result.ok().as_ref().and_then(|v| v["title"].as_str()) {
            store.activity.fetched_title(&sid, &bid, title);
        }
    });
    let (sid, bid) = (session_id.to_string(), block_id.to_string());
    store.call("com.superlogical.terminal.program_status", payload, cx, move |store, result, _| {
        if let Ok(v) = result {
            store.activity.fetched_status(&sid, &bid, &v);
        }
    });
}

// The window's side.

/// The window's own activity state.
#[derive(Debug, Default)]
pub struct Ui {
    /// The sidebar lists activity instead of sessions.
    pub sidebar: bool,
    /// Panes whose border flashes for a bell, until when.
    flash: HashMap<String, Instant>,
}

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    _target: &Target,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    match action {
        "pane.view_activity" => {
            let arg = |k: &str| args.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            let (mut session, block) = (arg("session"), arg("block"));
            if let (None, Some(b)) = (&session, &block) {
                // A block alone is enough: the tracker knows its session.
                session = ws.store.read(cx).activity.get(b).map(|b| b.session_id.clone()).filter(|s| !s.is_empty());
            }
            Some(match (session, block) {
                (Some(session), Some(block)) => go_to(ws, session, block, window, cx),
                _ => {
                    open_palette(ws, cx);
                    Ok(Value::Null)
                }
            })
        }
        "client.sidebar.activity.toggle" => {
            if ws.sidebar_visible {
                ws.activity.sidebar = !ws.activity.sidebar;
            } else {
                ws.sidebar_visible = true;
                ws.activity.sidebar = true;
            }
            cx.notify();
            Some(Ok(Value::Null))
        }
        _ => None,
    }
}

/// Shows a terminal: focuses it, which makes its tab the active one, then
/// shows its session.
///
/// The focus goes first, naming the session: a session shown for the first
/// time is attached by a request sent after it, so the view the attach
/// returns already has the terminal's tab active. Focusing after the attach
/// could leave the old tab showing, as the store ignores a session's view
/// changes until its attach returns.
fn go_to(ws: &mut Workspace, session: String, block: String, window: &mut Window, cx: &mut Context<Workspace>) -> Outcome {
    let payload = json!({"session_id": session, "block_id": block});
    ws.store.update(cx, |s, cx| s.request("session.focus_block", payload, cx));
    if ws.store.read(cx).current.as_deref() != Some(session.as_str()) {
        ws.perform("session.select", &json!({"session": session}), Target::default(), window, cx)?;
    }
    Ok(Value::Null)
}

fn open_palette(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let store = ws.store.read(cx);
    let items = store
        .activity
        .entries()
        .into_iter()
        .map(|e| {
            let place = location(store, &e, cx);
            let state = match (e.level, e.progress) {
                (Level::Working, Some(p)) => format!("working {p}%"),
                (level, _) => level.word().to_string(),
            };
            Item::new(shorten(&e.headline, 60), "pane.view_activity", json!({"session": e.session_id, "block": e.block_id}))
                .detail(format!("{} — {place}", shorten(&e.message, MESSAGE_CHARS)))
                .hint(state)
                .keywords(format!("{} {}", e.level.word(), e.level.group()))
        })
        .collect();
    ws.palette = Some(Palette::pick("Activity", items).empty("No program is doing anything"));
}

/// Where an entry is: its session, and its tab when the session is attached.
fn location(store: &Store, e: &Entry, cx: &App) -> String {
    let session = store.sessions.iter().find(|s| s.id == e.session_id).map(|s| s.label.clone()).unwrap_or_default();
    let tab = store
        .views
        .get(&e.session_id)
        .and_then(|v| v.windows.iter().find(|w| window_blocks(w).any(|b| b == e.block_id)))
        .map(|w| tab_title(w, store, cx));
    match tab {
        Some(tab) if !session.is_empty() => format!("{session} › {}", shorten(&tab, 40)),
        Some(tab) => shorten(&tab, 40),
        None => session,
    }
}

/// A tab's title: its focused terminal's title, or the tab's label.
pub fn tab_title(win: &RexWindow, store: &Store, cx: &App) -> String {
    let block = win.focused_block_id.as_deref().or_else(|| window_blocks(win).next());
    let title = block.and_then(|id| {
        store
            .activity
            .title(id)
            .map(str::to_string)
            .or_else(|| store.panes.get(id).map(|p| p.read(cx).title.clone()).filter(|t| !t.is_empty()))
    });
    title.unwrap_or_else(|| win.label.clone())
}

/// Marks what is on screen as seen, and flashes a pane whose bell just rang.
/// Runs as the window draws, so it sees what the user sees.
pub(crate) fn sync(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let now = Instant::now();
    ws.activity.flash.retain(|_, until| *until > now);
    if !window.is_window_active() {
        return;
    }
    let visible = ws.store.read(cx).current_view().and_then(active_window).map(visible_blocks).unwrap_or_default();
    if visible.is_empty() {
        return;
    }
    // No notify when something clears: this render, which draws every
    // badge, reads the tracker after this.
    let rang = ws.store.update(cx, |s, _| {
        let mut rang = Vec::new();
        for id in &visible {
            if s.activity.see(id, now).1 {
                rang.push(id.clone());
            }
        }
        rang
    });
    if !rang.is_empty() {
        for id in rang {
            ws.activity.flash.insert(id, now + FLASH);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }
}

/// A pane's border colour: amber while its bell flashes.
pub(crate) fn pane_border(ws: &Workspace, block_id: &str, normal: Hsla) -> Hsla {
    if ws.activity.flash.get(block_id).is_some_and(|until| *until > Instant::now()) { colors::amber() } else { normal }
}

/// Opens Rename Tab on a double click. Says whether it did.
pub(crate) fn tab_double_clicked(ws: &mut Workspace, window_id: &str, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Workspace>) -> bool {
    if ev.click_count() != 2 {
        return false;
    }
    let outcome = ws.perform("client.tab.rename", &json!({"window": window_id}), Target { origin: "mouse".into(), ..Default::default() }, window, cx);
    report(ws, "client.tab.rename", outcome, cx);
    true
}

/// Opens Rename Session on a double click. Says whether it did.
pub(crate) fn session_double_clicked(ws: &mut Workspace, session_id: &str, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Workspace>) -> bool {
    if ev.click_count() != 2 {
        return false;
    }
    let outcome = ws.perform("session.rename", &json!({"session": session_id}), Target { origin: "mouse".into(), ..Default::default() }, window, cx);
    report(ws, "session.rename", outcome, cx);
    true
}

fn report(ws: &mut Workspace, action: &str, outcome: Outcome, cx: &mut Context<Workspace>) {
    if let Err((_, message)) = outcome {
        let error = format!("{action}: {message}");
        ws.store.update(cx, |s, cx| {
            s.error = Some(error);
            cx.notify();
        });
    }
}

mod colors {
    use gpui::{Hsla, rgb};

    pub fn amber() -> Hsla {
        rgb(0xe0af68).into()
    }
    pub fn red() -> Hsla {
        rgb(0xf7768e).into()
    }
    pub fn green() -> Hsla {
        rgb(0x9ece6a).into()
    }
    pub fn accent() -> Hsla {
        rgb(0x7aa2f7).into()
    }
    pub fn dim() -> Hsla {
        rgb(0x6b7394).into()
    }
    pub fn text() -> Hsla {
        rgb(0xc0caf5).into()
    }
    pub fn track() -> Hsla {
        rgb(0x2a2c3d).into()
    }
}

fn dot(color: Hsla, size: f32) -> AnyElement {
    div().flex_shrink_0().size(px(size)).rounded_full().bg(color).into_any_element()
}

fn glyph(text: &'static str, color: Hsla) -> AnyElement {
    div().flex_shrink_0().text_size(px(11.0)).line_height(px(12.0)).text_color(color).child(text).into_any_element()
}

/// A small mark for a level: an amber dot for blocked, red for an error, a
/// thin bar (or a blue dot without a percentage) for working, a green check
/// for done, and a bell.
pub fn badge(level: Level, progress: Option<u8>) -> Option<AnyElement> {
    Some(match level {
        Level::Idle => return None,
        Level::Blocked => div()
            .flex_shrink_0()
            .size(px(9.0))
            .rounded_full()
            .border_2()
            .border_color(gpui::hsla(0.11, 0.66, 0.65, 0.35))
            .bg(colors::amber())
            .into_any_element(),
        Level::Error => dot(colors::red(), 7.0),
        Level::Working => match progress {
            Some(p) => div()
                .flex_shrink_0()
                .w(px(14.0))
                .h(px(3.0))
                .rounded_full()
                .bg(colors::track())
                .child(div().h_full().rounded_full().w(relative(p.max(6) as f32 / 100.0)).bg(colors::accent()))
                .into_any_element(),
            None => dot(colors::accent(), 6.0),
        },
        Level::Done => glyph("✓", colors::green()),
        Level::Bell => glyph("🔔", colors::amber()),
    })
}

/// The mark of a tab, from every terminal in it.
pub fn tab_badge(store: &Store, win: &RexWindow) -> Option<AnyElement> {
    let s = store.activity.summary(window_blocks(win))?;
    badge(s.level, s.progress)
}

/// The mark of a session in the sidebar, from every terminal in it.
pub fn session_badge(store: &Store, session_id: &str) -> Option<AnyElement> {
    let s = store.activity.session_summary(session_id)?;
    Some(div().flex().items_center().flex_shrink_0().w(px(16.0)).child(badge(s.level, s.progress)?).into_any_element())
}

/// The top of the sidebar: Sessions and Activity, the one showing brighter.
pub(crate) fn sidebar_header(ws: &Workspace, cx: &Context<Workspace>) -> impl IntoElement {
    let showing = ws.activity.sidebar;
    let count = ws.store.read(cx).activity.entries().len();
    let tab = |id: &'static str, label: String, activity: bool| {
        div()
            .id(id)
            .cursor_pointer()
            .text_color(if showing == activity { colors::text() } else { colors::dim() })
            .hover(|s| s.text_color(colors::text()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.activity.sidebar = activity;
                cx.notify();
            }))
            .child(SharedString::from(label))
    };
    let activity = if count > 0 { format!("ACTIVITY {count}") } else { "ACTIVITY".to_string() };
    div()
        .flex()
        .gap_3()
        .px_3()
        .py_2()
        .text_size(px(11.0))
        .child(tab("sidebar-sessions", "SESSIONS".to_string(), false))
        .child(tab("sidebar-activity", activity, true))
}

/// The sidebar's activity list: everything programs are doing, grouped by
/// how much it wants the user. Clicking one shows its terminal.
pub(crate) fn render_sidebar(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let store = ws.store.read(cx);
    let entries = store.activity.entries();
    let mut list = div().id("activity-list").flex().flex_col().flex_1().overflow_y_scroll();
    if entries.is_empty() {
        list = list.child(div().px_3().py_2().text_color(colors::dim()).text_size(px(12.0)).child("Nothing is happening."));
    }
    let mut group = None;
    for (i, e) in entries.iter().enumerate() {
        if group != Some(e.level) {
            group = Some(e.level);
            list = list.child(div().px_3().pt_2().pb_1().text_size(px(10.0)).text_color(colors::dim()).child(e.level.group().to_uppercase()));
        }
        let (session, block) = (e.session_id.clone(), e.block_id.clone());
        let place = location(store, e, cx);
        list = list.child(
            div()
                .id(("activity", i))
                .flex()
                .gap_2()
                .px_3()
                .py_1()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x24263a)))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let outcome = go_to(this, session.clone(), block.clone(), window, cx);
                    report(this, "pane.view_activity", outcome, cx);
                }))
                .child(div().flex().items_center().justify_center().flex_shrink_0().w(px(14.0)).h(px(17.0)).children(badge(e.level, e.progress)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_color(colors::text()).child(SharedString::from(e.headline.clone())))
                        .child(div().truncate().text_size(px(11.0)).text_color(colors::dim()).child(SharedString::from(shorten(&e.message, MESSAGE_CHARS))))
                        .child(div().truncate().text_size(px(10.0)).text_color(colors::dim()).child(SharedString::from(place))),
                ),
        );
    }
    div()
        .flex()
        .flex_col()
        .w(px(190.0))
        .flex_shrink_0()
        .h_full()
        .bg(rgb(0x1a1b26))
        .border_r_1()
        .border_color(rgb(0x2a2c3d))
        .child(sidebar_header(ws, cx))
        .child(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_with(events: &[(&str, Value)]) -> Tracker {
        let mut t = Tracker::default();
        for (name, p) in events {
            t.on_event("s", "b", name, p);
        }
        t
    }

    #[test]
    fn reads_records_and_their_levels() {
        let r = Record::from_json(None, &json!({"state": "blocked", "kind": "permission", "app": "x", "effective_app": "demo", "progress": 300, "updated_at": "t"})).unwrap();
        assert_eq!((r.id.as_str(), r.app.as_str(), r.progress, r.level()), ("", "demo", Some(100), Level::Blocked));
        assert_eq!(r.message(), "Waiting for permission");
        let unknown = Record::from_json(Some("a/b"), &json!({"state": "dancing"})).unwrap();
        assert_eq!((unknown.id.as_str(), unknown.level()), ("a/b", Level::Idle));
        assert!(Record::from_json(None, &json!({"msg": "no state"})).is_none());
        let mut done = Record::from_json(None, &json!({"state": "done", "msg": "Built"})).unwrap();
        assert_eq!(done.level(), Level::Done);
        done.seen = true;
        assert_eq!(done.level(), Level::Idle);
    }

    #[test]
    fn reads_progress_reports() {
        assert_eq!(Progress::from_report(&json!({"state": "set", "progress": 40})), Ok(Some(Progress { state: "set".into(), progress: Some(40) })));
        assert_eq!(Progress::from_report(&json!({"state": "indeterminate", "progress": 40})).unwrap().unwrap().progress, None);
        assert_eq!(Progress::from_report(&json!({"state": "error"})).unwrap().unwrap().level(), Level::Error);
        assert_eq!(Progress::from_report(&json!({"state": "remove"})), Ok(None));
        assert!(Progress::from_report(&json!({"state": "later"})).is_err());
    }

    #[test]
    fn status_events_replace_and_remove_records() {
        let mut t = tracker_with(&[
            ("program_status_changed", json!({"id": "", "record": {"state": "working", "app": "cargo"}, "reason": "report"})),
            ("program_status_changed", json!({"id": "test", "record": {"id": "test", "state": "blocked", "msg": "Approve?"}, "reason": "report"})),
        ]);
        let s = t.summary(["b"]).unwrap();
        assert_eq!((s.level, s.count), (Level::Blocked, 1));
        let entries = t.block_entries("b");
        assert_eq!(entries[0].message, "Approve?");
        assert_eq!(entries[1].headline, "cargo");
        t.on_event("s", "b", "program_status_removed", &json!({"ids": ["test"], "reason": "prompt"}));
        assert_eq!(t.summary(["b"]).unwrap().level, Level::Working);
        t.on_event("s", "b", "program_status_changed", &json!({"id": "", "record": {"state": "idle"}}));
        assert_eq!(t.summary(["b"]), None);
    }

    #[test]
    fn progress_reports_count_only_without_status_records() {
        let mut t = tracker_with(&[("progress_report", json!({"state": "set", "progress": 30}))]);
        assert_eq!(t.summary(["b"]), Some(Summary { level: Level::Working, progress: Some(30), count: 1 }));
        t.on_event("s", "b", "program_status_changed", &json!({"id": "", "record": {"state": "done"}}));
        assert_eq!(t.summary(["b"]).unwrap().level, Level::Done);
        t.on_event("s", "b", "progress_report", &json!({"state": "remove"}));
        assert!(t.get("b").unwrap().progress.is_none());
    }

    #[test]
    fn seeing_a_block_clears_done_bell_and_clean_exit_but_not_errors() {
        let mut t = tracker_with(&[
            ("program_status_changed", json!({"id": "", "record": {"state": "done"}})),
            ("bell", json!({})),
        ]);
        t.on_event("s", "c", "program_status_changed", &json!({"id": "", "record": {"state": "error"}}));
        let now = Instant::now();
        assert_eq!(t.see("b", now), (true, true));
        assert_eq!(t.see("b", now), (false, false));
        assert_eq!(t.summary(["b"]), None);
        t.see("c", now);
        assert_eq!(t.summary(["c"]).unwrap().level, Level::Error);
        // A new report of the same record is news again.
        t.on_event("s", "b", "program_status_changed", &json!({"id": "", "record": {"state": "done"}}));
        assert_eq!(t.summary(["b"]).unwrap().level, Level::Done);

        t.on_event("s", "d", "child_exited", &json!({"exit_code": 0}));
        t.on_event("s", "e", "child_exited", &json!({"exit_code": 2}));
        t.see("d", now);
        t.see("e", now);
        assert_eq!(t.summary(["d"]), None);
        assert_eq!(t.block_entries("e")[0].message, "Exited with code 2");
        t.on_event("s", "f", "child_exited", &json!({}));
        assert_eq!(t.block_entries("f")[0].message, "Ended by a signal");
    }

    #[test]
    fn an_old_bell_clears_without_flashing() {
        let mut t = tracker_with(&[("bell", json!({}))]);
        assert_eq!(t.see("b", Instant::now() + Duration::from_secs(5)), (true, false));
    }

    #[test]
    fn summaries_take_the_most_urgent_and_count_it() {
        let mut t = Tracker::default();
        t.on_event("s1", "a", "program_status_changed", &json!({"id": "", "record": {"state": "working", "progress": 10}}));
        t.on_event("s1", "b", "program_status_changed", &json!({"id": "", "record": {"state": "working"}}));
        t.on_event("s2", "c", "bell", &json!({}));
        assert_eq!(t.summary(["a", "b"]), Some(Summary { level: Level::Working, progress: Some(10), count: 2 }));
        assert_eq!(t.session_summary("s2").unwrap().level, Level::Bell);
        t.on_event("s2", "c", "program_status_changed", &json!({"id": "", "record": {"state": "error"}}));
        let all = t.entries();
        assert_eq!(all[0].level, Level::Error);
        assert_eq!(all.len(), 4);
        t.forget_session("s2");
        assert_eq!(t.session_summary("s2"), None);
        assert_eq!(t.entries().len(), 2);
    }

    #[test]
    fn fetched_titles_and_records_yield_to_events() {
        let mut t = tracker_with(&[
            ("title_changed", json!({"title": "vim\u{202E}"})),
            ("program_status_changed", json!({"id": "", "record": {"state": "working"}})),
        ]);
        t.fetched_title("s", "b", "older");
        assert_eq!(t.title("b"), Some("vim"));
        t.fetched_status("s", "b", &json!({"records": [{"state": "done"}, {"id": "x", "state": "blocked"}]}));
        let records = &t.get("b").unwrap().records;
        assert_eq!(records[""].state, "working");
        assert_eq!(records["x"].state, "blocked");
        t.fetched_title("s", "n", "fresh");
        assert_eq!(t.title("n"), Some("fresh"));
    }

    /// Events as a Rex server sent them, recorded with `rex events --json`,
    /// read the way the store reads a `session_event`.
    #[test]
    fn replays_events_as_the_server_sends_them() {
        let recorded = [
            r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"program_status_changed","payload":{"id":"","record":{"state":"working","progress":40,"app":"demo","effective_app":"demo","msg":"Building","updated_at":"2026-11-13T10:42:14.92Z","owner":{"pid":765776,"name":"bash"}},"reason":"report"}}}}}"#,
            r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"title_changed","payload":{"title":"demo title","owner":{"pid":765776,"name":"bash"}}}}}}"#,
            r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"progress_report","payload":{"state":"set","progress":55}}}}}"#,
            r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"bell","payload":{}}}}}"#,
            r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"program_status_changed","payload":{"id":"sub","record":{"id":"sub","state":"done","effective_app":"demo","title":"Sub","updated_at":"2026-11-13T10:42:14.93Z"},"reason":"report"}}}}}"#,
            r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:x","creator_name":"com.superlogical.terminal","name":"child_exited","payload":{"exit_code":3,"error":"exit status 3"}}}}}"#,
        ];
        let mut t = Tracker::default();
        for line in recorded {
            let ev: Value = serde_json::from_str(line).unwrap();
            let session_id = ev["payload"]["session_id"].as_str().unwrap();
            let p = &ev["payload"]["event"]["payload"];
            t.on_event(session_id, p["block_id"].as_str().unwrap(), p["name"].as_str().unwrap(), &p["payload"]);
        }
        assert_eq!(t.title("block:b"), Some("demo title"));
        let entries = t.block_entries("block:b");
        let seen: Vec<(Level, &str, &str, Option<u8>)> =
            entries.iter().map(|e| (e.level, e.headline.as_str(), e.message.as_str(), e.progress)).collect();
        assert_eq!(
            seen,
            vec![
                (Level::Working, "demo", "Building", Some(40)),
                (Level::Done, "demo · Sub", "Finished", None),
                (Level::Bell, "demo title", "Rang the bell", None),
            ]
        );
        assert_eq!(t.session_summary("session:s").unwrap().level, Level::Error);
        assert_eq!(t.block_entries("block:x")[0].message, "Exited with code 3");
    }

    #[test]
    fn the_activity_key_is_ctrl_shift_period() {
        let km = crate::keymap::Keymap::defaults();
        let mods = gpui::Modifiers { control: true, shift: true, ..Default::default() };
        let ks = gpui::Keystroke { modifiers: mods, key: ".".into(), key_char: None };
        let press = crate::keymap::press_of(&ks).unwrap();
        match km.resolve(&[], &[], &press, |_| true) {
            crate::keymap::Resolution::Run(b) => assert_eq!(b.action, "client.sidebar.activity.toggle"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cleans_and_shortens_program_text() {
        assert_eq!(sanitize(" a\u{202E}b\u{7}c\u{2066} "), "abc");
        assert_eq!(shorten("abcdef", 4), "abc…");
        assert_eq!(shorten("abc", 4), "abc");
    }

    #[test]
    fn visible_blocks_follow_zoom_and_minimized_layers() {
        let win: RexWindow = serde_json::from_value(json!({
            "window_id": "w",
            "layers": [
                {"kind": "tiled", "blocks": [{"block_id": "a"}, {"block_id": "b"}]},
                {"kind": "floating", "state": "minimized", "blocks": [{"block_id": "c"}]},
            ],
        }))
        .unwrap();
        assert_eq!(visible_blocks(&win), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(window_blocks(&win).count(), 3);
        let zoomed = RexWindow { zoomed_block_id: Some("b".into()), ..win };
        assert_eq!(visible_blocks(&zoomed), vec!["b".to_string()]);
    }
}
